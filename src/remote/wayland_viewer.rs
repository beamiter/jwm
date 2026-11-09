//! Native, read-only Wayland viewer. No seat, input, clipboard or X11 binding.
//! A buffer release only frees storage. Only wp_presentation.presented earns ACK.
use super::RemoteResult;
use super::frame::RecyclableDecodedFrame;
use super::window_size::WindowSize;
use super::x11_viewer::ViewerEvent;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::memfd::{MFdFlags, memfd_create};
use std::fs::File;
use std::io::{self, Seek, SeekFrom, Write};
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use wayland_client::backend::WaylandError;
use wayland_client::protocol::{
    wl_buffer, wl_callback, wl_compositor, wl_registry, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, WEnum};
use wayland_protocols::wp::presentation_time::client::{wp_presentation, wp_presentation_feedback};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

const TIMEOUT: Duration = Duration::from_secs(5);
const MAX_PIXELS: u64 = 16 * 1024 * 1024;
const MAX_RETAINED_BYTES: usize = 128 * 1024 * 1024;

fn dimensions(width: u16, height: u16) -> RemoteResult<()> {
    if width == 0
        || height == 0
        || width > 16384
        || height > 16384
        || u64::from(width) * u64::from(height) > MAX_PIXELS
    {
        return Err("Wayland viewer dimensions exceed bounded SHM capacity".into());
    }
    Ok(())
}
fn socket_path(display: Option<&str>) -> RemoteResult<PathBuf> {
    let name = display
        .map(str::to_owned)
        .or_else(|| std::env::var("WAYLAND_DISPLAY").ok())
        .unwrap_or_else(|| "wayland-0".into());
    if name.is_empty() || name.as_bytes().contains(&0) {
        return Err("invalid Wayland display".into());
    }
    let path = Path::new(&name);
    if path.is_absolute() {
        return Ok(path.to_owned());
    }
    if path.components().count() != 1 {
        return Err("Wayland display must be a single name or absolute path".into());
    }
    let runtime =
        PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").ok_or("missing XDG_RUNTIME_DIR")?);
    if !runtime.is_absolute() {
        return Err("XDG_RUNTIME_DIR must be absolute".into());
    }
    Ok(runtime.join(path))
}
struct Pending {
    token: u64,
    sequence: u64,
    source_epoch: u64,
    geometry_epoch: u64,
    generation: u64,
    presented: bool,
    started: Instant,
}
#[derive(Clone, Copy)]
struct SlotMeta {
    size: (u16, u16),
    bytes: usize,
    busy: bool,
}
struct Slot {
    id: u64,
    meta: SlotMeta,
    buffer: wl_buffer::WlBuffer,
    pool: wl_shm_pool::WlShmPool,
    file: File,
}
impl Drop for Slot {
    fn drop(&mut self) {
        self.buffer.destroy();
        self.pool.destroy();
    }
}
#[derive(Debug, PartialEq, Eq)]
enum SlotPlan {
    Reuse(usize),
    Allocate,
    Replace(usize),
}
type RetirementScope = (u64, (u16, u16), Option<(u64, u64)>);
fn retirement_scope_matches(before: RetirementScope, after: RetirementScope) -> bool {
    before == after
}
fn storage_can_submit(pending: bool, completed: bool, slots: usize, has_idle: bool) -> bool {
    !pending && !completed && (slots < 2 || (slots == 2 && has_idle))
}
fn plan_slot(slots: &[SlotMeta], size: (u16, u16)) -> RemoteResult<SlotPlan> {
    dimensions(size.0, size.1)?;
    if slots.len() > 2 {
        return Err("Wayland SHM slot count exceeded".into());
    }
    let total = slots
        .iter()
        .try_fold(0usize, |sum, slot| sum.checked_add(slot.bytes))
        .filter(|total| *total <= MAX_RETAINED_BYTES)
        .ok_or("Wayland SHM accounting overflow")?;
    if let Some(index) = slots
        .iter()
        .position(|slot| !slot.busy && slot.size == size)
    {
        return Ok(SlotPlan::Reuse(index));
    }
    let idle = slots.iter().position(|slot| !slot.busy);
    let bytes = usize::from(size.0) * usize::from(size.1) * 4;
    let retained = total - idle.map_or(0, |index| slots[index].bytes);
    if retained
        .checked_add(bytes)
        .is_none_or(|sum| sum > MAX_RETAINED_BYTES)
    {
        return Err("Wayland retained SHM exceeds 128 MiB".into());
    }
    match idle {
        Some(index) => Ok(SlotPlan::Replace(index)),
        None if slots.len() < 2 => Ok(SlotPlan::Allocate),
        None => Err("both Wayland SHM slots remain busy".into()),
    }
}
struct Retirement {
    id: u64,
    bytes: usize,
    started: Instant,
}
struct State {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    wm: Option<xdg_wm_base::XdgWmBase>,
    presentation: Option<wp_presentation::WpPresentation>,
    globals: Vec<u32>,
    xrgb: bool,
    configured: bool,
    width: u16,
    height: u16,
    proposed: Option<(u16, u16)>,
    generation: u64,
    scope: Option<(u64, u64)>,
    pending: Option<Pending>,
    slots: Vec<Slot>,
    retirement: Option<Retirement>,
    blocked_since: Option<Instant>,
    completed: Vec<u64>,
    closed: bool,
    failure: Option<String>,
}
impl State {
    fn check(&self) -> RemoteResult<()> {
        if let Some(error) = &self.failure {
            return Err(error.clone().into());
        }
        if let Some(retirement) = &self.retirement {
            if retirement.started.elapsed() >= TIMEOUT {
                return Err("Wayland SHM retirement deadline expired".into());
            }
            let total = self.slots.iter().try_fold(retirement.bytes, |sum, slot| {
                sum.checked_add(slot.meta.bytes)
            });
            if total.is_none_or(|bytes| bytes > MAX_RETAINED_BYTES) {
                return Err("Wayland retiring SHM exceeds 128 MiB".into());
            }
        }
        if self
            .pending
            .as_ref()
            .is_some_and(|p| p.started.elapsed() >= TIMEOUT)
        {
            return Err("Wayland presentation deadline expired".into());
        }
        if self.blocked_since.is_some_and(|at| at.elapsed() >= TIMEOUT)
            && self.slots.iter().filter(|p| p.meta.busy).count() >= 2
        {
            return Err("both Wayland SHM slots remained busy past deadline".into());
        }
        Ok(())
    }
    fn advance_generation(&mut self) {
        self.generation = match self.generation.checked_add(1) {
            Some(next) => next,
            None => {
                self.failure = Some("Wayland surface generation exhausted".into());
                return;
            }
        };
        if self.pending.as_ref().is_some_and(|p| !p.presented) || !self.completed.is_empty() {
            // The existing client ACK ledger cannot retire a discarded sequence.
            // Fail closed rather than pretending an obsolete commit was drawn.
            self.failure = Some("Wayland surface changed before presentation completed".into());
        }
        self.completed.clear();
    }
}

pub(super) struct WaylandViewer {
    conn: Connection,
    queue: EventQueue<State>,
    state: State,
    surface: wl_surface::WlSurface,
    _xdg_surface: xdg_surface::XdgSurface,
    toplevel: xdg_toplevel::XdgToplevel,
    token: u64,
    slot_identity: u64,
    write_pending: std::cell::Cell<bool>,
}
impl WaylandViewer {
    pub(super) fn connect(
        display: Option<&str>,
        width: u16,
        height: u16,
        window_size: Option<WindowSize>,
        title: &str,
    ) -> RemoteResult<Self> {
        let stream = jwm_ipc_transport::connect(&socket_path(display)?, TIMEOUT)?;
        Self::connect_stream(stream, width, height, window_size, title)
    }
    #[cfg(test)]
    pub(crate) fn connect_test_stream(
        stream: UnixStream,
        width: u16,
        height: u16,
        window_size: Option<WindowSize>,
        title: &str,
    ) -> RemoteResult<Self> {
        Self::connect_stream(stream, width, height, window_size, title)
    }
    fn connect_stream(
        stream: UnixStream,
        width: u16,
        height: u16,
        window_size: Option<WindowSize>,
        title: &str,
    ) -> RemoteResult<Self> {
        let (width, height) = window_size.map_or((width, height), |s| (s.width, s.height));
        dimensions(width, height)?;
        if title.len() > 1024 || title.as_bytes().contains(&0) {
            return Err("invalid Wayland viewer title".into());
        }
        stream.set_nonblocking(true)?;
        let conn = Connection::from_socket(stream)?;
        let mut queue = conn.new_event_queue();
        let qh = queue.handle();
        let _registry = conn.display().get_registry(&qh, ());
        let mut state = State {
            compositor: None,
            shm: None,
            wm: None,
            presentation: None,
            globals: Vec::new(),
            xrgb: false,
            configured: false,
            width,
            height,
            proposed: None,
            generation: 1,
            scope: None,
            pending: None,
            slots: Vec::new(),
            retirement: None,
            blocked_since: None,
            completed: Vec::new(),
            closed: false,
            failure: None,
        };
        let deadline = Instant::now() + TIMEOUT;
        // Bounded nonblocking pumping instead of an unbounded roundtrip.
        while state.compositor.is_none()
            || state.shm.is_none()
            || state.wm.is_none()
            || state.presentation.is_none()
            || !state.xrgb
        {
            pump_connection(&conn, &mut queue, &mut state)?;
            if Instant::now() >= deadline {
                return Err(
                    "Wayland viewer requires compositor, XRGB SHM, xdg-shell and wp_presentation"
                        .into(),
                );
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        let surface = state.compositor.as_ref().unwrap().create_surface(&qh, ());
        surface.set_buffer_scale(1);
        let xdg_surface = state
            .wm
            .as_ref()
            .unwrap()
            .get_xdg_surface(&surface, &qh, ());
        let toplevel = xdg_surface.get_toplevel(&qh, ());
        toplevel.set_title(title.to_owned());
        toplevel.set_app_id("jwm-remote-readonly".into());
        surface.commit();
        while !state.configured {
            pump_connection(&conn, &mut queue, &mut state)?;
            if state.closed {
                return Err("Wayland viewer closed during configuration".into());
            }
            if Instant::now() >= deadline {
                return Err("Wayland viewer configure deadline expired".into());
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        Ok(Self {
            conn,
            queue,
            state,
            surface,
            _xdg_surface: xdg_surface,
            toplevel,
            token: 0,
            slot_identity: 0,
            write_pending: std::cell::Cell::new(false),
        })
    }
    fn pump(&mut self) -> RemoteResult<bool> {
        let work = pump_connection(&self.conn, &mut self.queue, &mut self.state)?;
        if self.state.pending.as_ref().is_some_and(|p| p.presented) {
            self.state.pending = None;
        }
        // Released storage remains allocated and charged until reused or safely
        // replaced. Presentation alone never makes its bytes writable.
        if self
            .state
            .slots
            .iter()
            .filter(|slot| slot.meta.busy)
            .count()
            >= 2
        {
            self.state.blocked_since.get_or_insert_with(Instant::now);
        } else {
            self.state.blocked_since = None;
        }
        Ok(work)
    }
    pub(super) fn poll_events(&mut self) -> RemoteResult<Vec<ViewerEvent>> {
        self.pump()?;
        Ok(if self.state.closed {
            vec![ViewerEvent::Close]
        } else {
            Vec::new()
        })
    }
    pub(super) fn viewport(&self) -> (u16, u16) {
        (self.state.width, self.state.height)
    }
    pub(super) fn can_submit_frame(&self) -> bool {
        !self.state.closed
            && self.state.retirement.is_none()
            && storage_can_submit(
                self.state.pending.is_some(),
                !self.state.completed.is_empty(),
                self.state.slots.len(),
                self.state.slots.iter().any(|slot| !slot.meta.busy),
            )
            && self.state.failure.is_none()
    }
    pub(super) fn draw_recyclable(&mut self, frame: RecyclableDecodedFrame) -> RemoteResult<bool> {
        self.pump()?;
        if !self.can_submit_frame() {
            return Ok(false);
        }
        let scope = (frame.source_epoch(), frame.geometry_epoch());
        if self.state.scope.is_some_and(|old| old.0 != scope.0) {
            return self.invalidate_source(String::new()).map(|()| false);
        }
        if self.state.scope.is_some_and(|old| old != scope) {
            self.state.advance_generation();
        }
        self.state.check()?;
        self.state.scope = Some(scope);
        let image = frame.image();
        if image.width() == 0 || image.height() == 0 {
            return Err("empty Wayland frame".into());
        }
        super::frame::validate_dimensions(
            u16::try_from(image.width())?,
            u16::try_from(image.height())?,
        )?;
        let (width, height) = self.viewport();
        dimensions(width, height)?;
        let bytes = usize::from(width) * usize::from(height) * 4;
        let metas: Vec<_> = self.state.slots.iter().map(|slot| slot.meta).collect();
        let plan = plan_slot(&metas, (width, height))?;
        let index = match plan {
            SlotPlan::Reuse(index) => index,
            SlotPlan::Replace(index) => {
                // Destroy and close idle old-size storage BEFORE allocating;
                // temporary allocations must not exceed the same byte budget.
                self.retire_slot(index)?;
                self.allocate_slot(width, height, bytes)?
            }
            SlotPlan::Allocate => self.allocate_slot(width, height, bytes)?,
        };
        let slot = &mut self.state.slots[index];
        debug_assert!(!slot.meta.busy);
        slot.file.seek(SeekFrom::Start(0))?;
        // One native-endian XRGB8888 upload, letterboxed without an extra scaled canvas.
        let (iw, ih) = (u64::from(image.width()), u64::from(image.height()));
        let (dw, dh) = if u64::from(width) * ih <= u64::from(height) * iw {
            (u32::from(width), (u64::from(width) * ih / iw).max(1) as u32)
        } else {
            (
                (u64::from(height) * iw / ih).max(1) as u32,
                u32::from(height),
            )
        };
        let (ox, oy) = ((u32::from(width) - dw) / 2, (u32::from(height) - dh) / 2);
        let mut row = vec![0u8; usize::from(width) * 4];
        for y in 0..u32::from(height) {
            row.fill(0);
            if y >= oy && y < oy + dh {
                let sy = ((u64::from(y - oy) * ih) / u64::from(dh)) as u32;
                for x in 0..dw {
                    let sx = ((u64::from(x) * iw) / u64::from(dw)) as u32;
                    let rgb = image.get_pixel(sx, sy).0;
                    let pixel =
                        (u32::from(rgb[0]) << 16) | (u32::from(rgb[1]) << 8) | u32::from(rgb[2]);
                    let at = ((x + ox) * 4) as usize;
                    row[at..at + 4].copy_from_slice(&pixel.to_ne_bytes());
                }
            }
            slot.file.write_all(&row)?;
        }
        self.token = self
            .token
            .checked_add(1)
            .ok_or("Wayland viewer token exhausted")?;
        let qh = self.queue.handle();
        let buffer = slot.buffer.clone();
        slot.meta.busy = true;
        let _feedback =
            self.state
                .presentation
                .as_ref()
                .unwrap()
                .feedback(&self.surface, &qh, self.token);
        self.state.pending = Some(Pending {
            token: self.token,
            sequence: frame.sequence(),
            source_epoch: scope.0,
            geometry_epoch: scope.1,
            generation: self.state.generation,
            presented: false,
            started: Instant::now(),
        });
        self.surface.attach(Some(&buffer), 0, 0);
        self.surface
            .damage_buffer(0, 0, i32::from(width), i32::from(height));
        self.surface.commit();
        self.prepare_wait()?;
        Ok(true)
    }
    fn retire_slot(&mut self, index: usize) -> RemoteResult<()> {
        if self.state.retirement.is_some() || self.state.slots[index].meta.busy {
            return Err("cannot retire busy Wayland SHM storage".into());
        }
        let snapshot = (self.state.generation, self.viewport(), self.state.scope);
        let slot = self.state.slots.remove(index);
        self.state.retirement = Some(Retirement {
            id: slot.id,
            bytes: slot.meta.bytes,
            started: Instant::now(),
        });
        let id = slot.id;
        // Merely dropping the local fd is not a server-side memory fence.
        // Keep these bytes charged until the server has processed both destroys.
        drop(slot);
        self.conn.display().sync(&self.queue.handle(), id);
        while self.state.retirement.is_some() {
            self.prepare_wait()?;
            pump_connection(&self.conn, &mut self.queue, &mut self.state)?;
            if self.state.closed
                || !retirement_scope_matches(
                    snapshot,
                    (self.state.generation, self.viewport(), self.state.scope),
                )
            {
                return Err("Wayland viewer changed while retiring SHM storage".into());
            }
            self.state.check()?;
            if self.state.retirement.is_some() {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        Ok(())
    }
    fn allocate_slot(&mut self, width: u16, height: u16, bytes: usize) -> RemoteResult<usize> {
        let id = self
            .slot_identity
            .checked_add(1)
            .ok_or("Wayland buffer identity exhausted")?;
        let file = File::from(memfd_create(c"jwm-readonly-viewer", MFdFlags::MFD_CLOEXEC)?);
        file.set_len(u64::try_from(bytes)?)?;
        let qh = self.queue.handle();
        let pool = self.state.shm.as_ref().unwrap().create_pool(
            file.as_fd(),
            i32::try_from(bytes)?,
            &qh,
            (),
        );
        let buffer = pool.create_buffer(
            0,
            i32::from(width),
            i32::from(height),
            i32::from(width) * 4,
            wl_shm::Format::Xrgb8888,
            &qh,
            id,
        );
        self.slot_identity = id;
        let index = self.state.slots.len();
        self.state.slots.push(Slot {
            id,
            meta: SlotMeta {
                size: (width, height),
                bytes,
                busy: false,
            },
            buffer,
            pool,
            file,
        });
        Ok(index)
    }
    pub(super) fn take_presented_sequences(&mut self) -> RemoteResult<Vec<u64>> {
        self.pump()?;
        Ok(std::mem::take(&mut self.state.completed))
    }
    pub(super) fn prepare_wait(&self) -> RemoteResult<()> {
        match self.conn.flush() {
            Ok(()) => self.write_pending.set(false),
            Err(WaylandError::Io(e)) if e.kind() == io::ErrorKind::WouldBlock => {
                self.write_pending.set(true)
            }
            Err(e) => return Err(e.into()),
        }
        Ok(())
    }
    pub(super) fn has_pending_events(&mut self) -> RemoteResult<bool> {
        let work = self.pump()?;
        Ok(work || self.state.closed || !self.state.completed.is_empty())
    }
    pub(super) fn connection_fd(&self) -> BorrowedFd<'_> {
        self.conn.as_fd()
    }
    pub(super) fn next_event_deadline(&self) -> Option<Instant> {
        let pending = self
            .state
            .pending
            .as_ref()
            .map(|p| p.started + TIMEOUT)
            .or_else(|| self.state.blocked_since.map(|at| at + TIMEOUT));
        if self.write_pending.get() {
            Some(
                pending.map_or(Instant::now() + Duration::from_millis(5), |p| {
                    p.min(Instant::now() + Duration::from_millis(5))
                }),
            )
        } else {
            pending
        }
    }
    pub(super) fn user_close_requested(&self) -> bool {
        self.state.closed
    }
    pub(super) fn prepare_frame_geometry(&mut self, epoch: u64) -> RemoteResult<bool> {
        let changed = self.state.scope.is_some_and(|(_, old)| old != epoch);
        if changed {
            self.state.advance_generation();
            self.state.check()?;
        }
        Ok(changed)
    }
    pub(super) fn refresh_pending_redraw(&mut self) -> RemoteResult<()> {
        self.pump()?;
        Ok(())
    }
    pub(super) fn invalidate_source(&mut self, _label: String) -> RemoteResult<()> {
        // A source switch would require a complete xdg unmap/remap handshake.
        // First-stage native viewer deliberately does not support that flow.
        self.state.failure =
            Some("Wayland viewer source switching is unsupported; session closed".into());
        self.state.completed.clear();
        self.state.check()
    }
    pub(super) fn source_presented(&mut self, _epoch: u64) {}
    pub(super) fn set_source_label(&mut self, label: String) {
        self.toplevel.set_title(
            label
                .chars()
                .filter(|c| !c.is_control())
                .take(256)
                .collect(),
        );
    }
}
fn pump_connection(
    conn: &Connection,
    queue: &mut EventQueue<State>,
    state: &mut State,
) -> RemoteResult<bool> {
    let mut dispatched = queue.dispatch_pending(state)?;
    state.check()?;
    let backend = conn.backend();
    dispatched += backend.dispatch_inner_queue()?;
    dispatched += queue.dispatch_pending(state)?;
    let Some(guard) = backend.prepare_read() else {
        return Ok(true);
    };
    let needs_write = match backend.flush() {
        Ok(()) => false,
        Err(WaylandError::Io(e)) if e.kind() == io::ErrorKind::WouldBlock => true,
        Err(e) => return Err(e.into()),
    };
    let mut flags = PollFlags::POLLIN | PollFlags::POLLERR | PollFlags::POLLHUP;
    if needs_write {
        flags |= PollFlags::POLLOUT;
    }
    let mut fds = [PollFd::new(backend.poll_fd(), flags)];
    match poll(&mut fds, PollTimeout::ZERO) {
        Ok(_) => {}
        Err(nix::errno::Errno::EINTR) => return Ok(dispatched > 0),
        Err(e) => return Err(e.into()),
    }
    let ready = fds[0].revents().unwrap_or(PollFlags::empty());
    if ready.intersects(PollFlags::POLLERR | PollFlags::POLLHUP | PollFlags::POLLNVAL) {
        return Err("Wayland viewer disconnected".into());
    }
    if ready.contains(PollFlags::POLLIN) {
        match guard.read() {
            Ok(_) => {}
            Err(WaylandError::Io(e)) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e.into()),
        }
    } else {
        drop(guard);
    }
    dispatched += queue.dispatch_pending(state)?;
    state.check()?;
    Ok(dispatched > 0)
}
impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => {
                match interface.as_str() {
                    "wl_compositor" if state.compositor.is_none() && version >= 4 => {
                        state.compositor = Some(registry.bind(name, version.min(6), qh, ()))
                    }
                    "wl_shm" if state.shm.is_none() => {
                        state.shm = Some(registry.bind(name, 1, qh, ()))
                    }
                    "xdg_wm_base" if state.wm.is_none() => {
                        state.wm = Some(registry.bind(name, version.min(6), qh, ()))
                    }
                    "wp_presentation" if state.presentation.is_none() => {
                        state.presentation = Some(registry.bind(name, 1, qh, ()))
                    }
                    _ => return,
                }
                if state.globals.len() >= 4 {
                    state.failure = Some("duplicate Wayland globals".into());
                } else {
                    state.globals.push(name);
                }
            }
            wl_registry::Event::GlobalRemove { name } if state.globals.contains(&name) => {
                state.failure = Some("required Wayland global removed".into())
            }
            _ => {}
        }
    }
}
impl Dispatch<wl_shm::WlShm, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_shm::WlShm,
        event: wl_shm::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_shm::Event::Format {
            format: WEnum::Value(wl_shm::Format::Xrgb8888),
        } = event
        {
            state.xrgb = true;
        }
    }
}
impl Dispatch<xdg_wm_base::XdgWmBase, ()> for State {
    fn event(
        _: &mut Self,
        wm: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm.pong(serial);
        }
    }
}
impl Dispatch<xdg_toplevel::XdgToplevel, ()> for State {
    fn event(
        state: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            xdg_toplevel::Event::Configure { width, height, .. } => {
                let width = if width == 0 {
                    Some(state.width)
                } else {
                    u16::try_from(width).ok()
                };
                let height = if height == 0 {
                    Some(state.height)
                } else {
                    u16::try_from(height).ok()
                };
                match width.zip(height) {
                    Some((w, h)) if dimensions(w, h).is_ok() => state.proposed = Some((w, h)),
                    _ => state.failure = Some("invalid Wayland configured dimensions".into()),
                }
            }
            xdg_toplevel::Event::Close => state.closed = true,
            _ => {}
        }
    }
}
impl Dispatch<xdg_surface::XdgSurface, ()> for State {
    fn event(
        state: &mut Self,
        surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            if let Some((w, h)) = state.proposed.take() {
                if state.configured && (w, h) != (state.width, state.height) {
                    state.advance_generation();
                }
                state.width = w;
                state.height = h;
            }
            state.configured = true;
            surface.ack_configure(serial);
        }
    }
}
impl Dispatch<wl_surface::WlSurface, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_surface::WlSurface,
        event: wl_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_surface::Event::PreferredBufferScale { factor } if factor != 1 => {
                state.failure = Some("Wayland viewer only supports buffer scale one".into())
            }
            wl_surface::Event::PreferredBufferTransform { transform }
                if transform
                    != WEnum::Value(wayland_client::protocol::wl_output::Transform::Normal) =>
            {
                state.failure = Some("Wayland viewer only supports normal transform".into())
            }
            _ => {}
        }
    }
}
impl Dispatch<wl_callback::WlCallback, u64> for State {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        event: wl_callback::Event,
        id: &u64,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            match state.retirement.as_ref() {
                Some(retirement)
                    if retirement.id == *id && retirement.started.elapsed() < TIMEOUT =>
                {
                    state.retirement = None;
                }
                _ => state.failure = Some("stale or expired Wayland SHM retirement fence".into()),
            }
        }
    }
}
impl Dispatch<wl_buffer::WlBuffer, u64> for State {
    fn event(
        state: &mut Self,
        _: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        token: &u64,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event {
            match state.slots.iter_mut().find(|slot| slot.id == *token) {
                Some(slot) if slot.meta.busy => slot.meta.busy = false,
                _ => state.failure = Some("stale or duplicate Wayland buffer release".into()),
            }
        }
    }
}
impl Dispatch<wp_presentation_feedback::WpPresentationFeedback, u64> for State {
    fn event(
        state: &mut Self,
        _: &wp_presentation_feedback::WpPresentationFeedback,
        event: wp_presentation_feedback::Event,
        token: &u64,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wp_presentation_feedback::Event::Presented { .. } => match state.pending.as_mut() {
                Some(p)
                    if p.token == *token
                        && !p.presented
                        && p.generation == state.generation
                        && state.scope == Some((p.source_epoch, p.geometry_epoch)) =>
                {
                    p.presented = true;
                    if state.completed.is_empty() {
                        state.completed.push(p.sequence);
                    } else {
                        state.failure = Some("unconsumed Wayland presentation completion".into());
                    }
                }
                _ => {
                    state.failure = Some("stale or duplicate Wayland presentation feedback".into())
                }
            },
            wp_presentation_feedback::Event::Discarded => {
                state.failure =
                    Some("Wayland compositor discarded frame; no presentation ACK".into())
            }
            wp_presentation_feedback::Event::SyncOutput { .. } => {}
            _ => state.failure = Some("unsupported Wayland presentation feedback".into()),
        }
    }
}
wayland_client::delegate_noop!(State: ignore wl_compositor::WlCompositor);
wayland_client::delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
wayland_client::delegate_noop!(State: ignore wp_presentation::WpPresentation);

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retirement_cannot_rehabilitate_changed_generation_viewport_or_source() {
        let before = (7, (800, 600), Some((1, 3)));
        assert!(retirement_scope_matches(before, before));
        for after in [
            (8, (800, 600), Some((1, 3))),
            (7, (640, 480), Some((1, 3))),
            (7, (800, 600), Some((2, 3))),
            (7, (800, 600), Some((1, 4))),
        ] {
            assert!(!retirement_scope_matches(before, after));
        }
    }
    fn slot(size: (u16, u16), busy: bool) -> SlotMeta {
        SlotMeta {
            size,
            bytes: usize::from(size.0) * usize::from(size.1) * 4,
            busy,
        }
    }
    #[test]
    fn released_storage_is_reused_but_busy_storage_never_is() {
        let size = (800, 600);
        assert_eq!(
            plan_slot(&[slot(size, false)], size).unwrap(),
            SlotPlan::Reuse(0)
        );
        assert_eq!(
            plan_slot(&[slot(size, true)], size).unwrap(),
            SlotPlan::Allocate
        );
        assert_eq!(
            plan_slot(&[slot(size, true), slot(size, false)], size).unwrap(),
            SlotPlan::Reuse(1)
        );
        assert!(plan_slot(&[slot(size, true), slot(size, true)], size).is_err());
    }
    #[test]
    fn resize_replaces_only_idle_slot_and_keeps_busy_old_size_charged() {
        let old = (4096, 4096);
        let size = (800, 600);
        assert_eq!(
            plan_slot(&[slot(old, true), slot(old, false)], size).unwrap(),
            SlotPlan::Replace(1)
        );
        assert_eq!(
            plan_slot(&[slot(old, true)], old).unwrap(),
            SlotPlan::Allocate
        );
        assert!(plan_slot(&[slot(old, true), slot(old, true)], size).is_err());
        assert!(plan_slot(&[], (4097, 4096)).is_err());
    }
    #[test]
    fn accounting_includes_idle_and_detects_overflow_and_slot_excess() {
        let corrupt = SlotMeta {
            size: (1, 1),
            bytes: usize::MAX,
            busy: false,
        };
        assert!(plan_slot(&[corrupt, slot((1, 1), false)], (1, 1)).is_err());
        assert!(plan_slot(&[slot((1, 1), false); 3], (1, 1)).is_err());
        assert!(
            plan_slot(
                &[SlotMeta {
                    bytes: MAX_RETAINED_BYTES + 1,
                    ..slot((1, 1), false)
                }],
                (1, 1)
            )
            .is_err()
        );
    }
    #[test]
    fn release_does_not_bypass_pending_feedback_or_unconsumed_ack() {
        assert!(!storage_can_submit(true, false, 1, true));
        assert!(!storage_can_submit(false, true, 1, true));
        assert!(storage_can_submit(false, false, 1, false));
        assert!(!storage_can_submit(false, false, 2, false));
        assert!(storage_can_submit(false, false, 2, true));
        assert!(!storage_can_submit(false, false, 3, true));
    }

    #[test]
    fn native_viewer_shm_dimensions_are_bounded() {
        assert!(dimensions(1, 1).is_ok());
        assert!(dimensions(4096, 4096).is_ok());
        assert!(dimensions(0, 600).is_err());
        assert!(dimensions(16385, 1).is_err());
        assert!(dimensions(4097, 4096).is_err());
    }
    #[test]
    fn explicit_display_rejects_empty_or_relative_paths_without_environment_fallback() {
        assert!(socket_path(Some("")).is_err());
        assert!(socket_path(Some("nested/wayland-0")).is_err());
        assert!(socket_path(Some("bad\0name")).is_err());
        assert_eq!(
            socket_path(Some("/owned/test/socket")).unwrap(),
            PathBuf::from("/owned/test/socket")
        );
    }
}
