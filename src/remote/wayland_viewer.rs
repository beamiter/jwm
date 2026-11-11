//! Native Wayland viewer, read-only unless one explicit scoped input mode is enabled.
//! Independent input modes stay exclusive. Explicit control observes both devices
//! on one seat with cross-device exclusion; no clipboard, grab or X11 binding.
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

#[path = "wayland_viewer_keyboard.rs"]
mod keyboard;
#[path = "wayland_viewer_pointer.rs"]
mod pointer;
#[cfg(all(test, feature = "backend-wayland-udev"))]
#[path = "wayland_viewer_popup_presentation.rs"]
pub(crate) mod popup_presentation;

#[cfg(all(test, feature = "backend-wayland-udev"))]
thread_local! {
    static POINTER_BUTTON_DISPATCH_OBSERVER: std::cell::RefCell<Option<std::sync::Arc<std::sync::Mutex<(u64,u64)>>>> = const { std::cell::RefCell::new(None) };
}
/// Counts actual left-button Dispatch entries, including revoked early returns.
#[cfg(all(test, feature = "backend-wayland-udev"))]
pub(crate) fn with_pointer_button_dispatch_observer<T>(
    observer: std::sync::Arc<std::sync::Mutex<(u64, u64)>>,
    run: impl FnOnce() -> T,
) -> T {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            POINTER_BUTTON_DISPATCH_OBSERVER.with(|s| {
                s.borrow_mut().take();
            });
        }
    }
    POINTER_BUTTON_DISPATCH_OBSERVER.with(|s| {
        assert!(s.borrow().is_none());
        *s.borrow_mut() = Some(observer);
    });
    let _reset = Reset;
    run()
}

// Only observes successful commits and actually drained presented sequences.
#[cfg(all(test, feature = "backend-wayland-udev"))]
type ControlFrameObservation = std::sync::Arc<std::sync::Mutex<(Option<u64>, Option<u64>)>>;
#[cfg(all(test, feature = "backend-wayland-udev"))]
thread_local! {
    static CONTROL_FRAME_OBSERVER: std::cell::RefCell<Option<ControlFrameObservation>> = const { std::cell::RefCell::new(None) };
}
#[cfg(all(test, feature = "backend-wayland-udev"))]
pub(crate) fn with_wayland_control_frame_observer<T>(
    observer: ControlFrameObservation,
    run: impl FnOnce() -> T,
) -> T {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            CONTROL_FRAME_OBSERVER.with(|slot| {
                slot.borrow_mut().take();
            });
        }
    }
    CONTROL_FRAME_OBSERVER.with(|slot| {
        assert!(slot.borrow().is_none());
        *slot.borrow_mut() = Some(observer);
    });
    let _reset = Reset;
    run()
}
#[cfg(all(test, feature = "backend-wayland-udev"))]
thread_local! {
    static CONTROL_SCROLL_OBSERVER: std::cell::RefCell<Option<std::sync::Arc<std::sync::atomic::AtomicU64>>> = const { std::cell::RefCell::new(None) };
}
/// Observe consumed native axis groups, including dropped Waiting groups.
/// This count cannot authorize, present or enqueue an action.
#[cfg(all(test, feature = "backend-wayland-udev"))]
pub(crate) fn with_wayland_control_scroll_observer<T>(
    observer: std::sync::Arc<std::sync::atomic::AtomicU64>,
    run: impl FnOnce() -> T,
) -> T {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            CONTROL_SCROLL_OBSERVER.with(|slot| {
                slot.borrow_mut().take();
            });
        }
    }
    CONTROL_SCROLL_OBSERVER.with(|slot| {
        assert!(slot.borrow().is_none());
        *slot.borrow_mut() = Some(observer);
    });
    let _reset = Reset;
    run()
}
/// Test-only ledger of actual drained presentation completions, never feedback injection.
#[cfg(all(test, feature = "backend-wayland-udev"))]
type PresentedTimeObservation = std::sync::Arc<std::sync::Mutex<Vec<(u64, Instant)>>>;
#[cfg(all(test, feature = "backend-wayland-udev"))]
thread_local! {
    static PRESENTED_TIME_OBSERVER: std::cell::RefCell<Option<PresentedTimeObservation>> = const { std::cell::RefCell::new(None) };
}
#[cfg(all(test, feature = "backend-wayland-udev"))]
pub(crate) fn with_wayland_presented_time_observer<T>(
    observer: PresentedTimeObservation,
    run: impl FnOnce() -> T,
) -> T {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            PRESENTED_TIME_OBSERVER.with(|slot| {
                slot.borrow_mut().take();
            });
        }
    }
    PRESENTED_TIME_OBSERVER.with(|slot| {
        assert!(slot.borrow().is_none());
        *slot.borrow_mut() = Some(observer);
    });
    let _reset = Reset;
    run()
}
const TIMEOUT: Duration = Duration::from_secs(5);
const MAX_PIXELS: u64 = 16 * 1024 * 1024;
const MAX_RETAINED_BYTES: usize = 128 * 1024 * 1024;
// Separate bounded upload working storage, not part of the SHM allocation budget.
const MAX_UPLOAD_SCRATCH_BYTES: usize = 256 * 1024;

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
/// Upload complete native-endian XRGB rows, including black letterbox pixels.
/// All scratch is caller-owned and reused; write_all handles partial writes and
/// Interrupted. No presentation or buffer lifecycle changes happen in this helper.
fn upload_xrgb(
    writer: &mut impl Write,
    image: &image::RgbImage,
    width: u16,
    height: u16,
    scratch: &mut [u8],
) -> io::Result<()> {
    dimensions(width, height)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;
    if image.width() == 0 || image.height() == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "empty upload image",
        ));
    }
    let row_bytes = usize::from(width) * 4;
    if scratch.len() > MAX_UPLOAD_SCRATCH_BYTES || scratch.len() < row_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid upload scratch size",
        ));
    }
    let rows_per_chunk = scratch.len() / row_bytes;
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
    for first_y in (0..usize::from(height)).step_by(rows_per_chunk) {
        let rows = rows_per_chunk.min(usize::from(height) - first_y);
        let chunk = &mut scratch[..rows * row_bytes];
        chunk.fill(0);
        for local_y in 0..rows {
            let y = (first_y + local_y) as u32;
            if y < oy || y >= oy + dh {
                continue;
            }
            let sy = ((u64::from(y - oy) * ih) / u64::from(dh)) as u32;
            for x in 0..dw {
                let sx = ((u64::from(x) * iw) / u64::from(dw)) as u32;
                let rgb = image.get_pixel(sx, sy).0;
                let pixel =
                    (u32::from(rgb[0]) << 16) | (u32::from(rgb[1]) << 8) | u32::from(rgb[2]);
                let at = local_y * row_bytes + ((x + ox) * 4) as usize;
                chunk[at..at + 4].copy_from_slice(&pixel.to_ne_bytes());
            }
        }
        writer.write_all(chunk)?;
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
    mapping: pointer::Mapping,
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
fn control_repair_busy(
    keyboard: &keyboard::Keyboard,
    pointer: &pointer::Pointer,
    collected: &[ViewerEvent],
) -> bool {
    keyboard.revoked.is_some()
        || keyboard.pending_keys()
        || pointer.repair_busy()
        || !collected.is_empty()
}
impl State {
    fn control_collect(&mut self) {
        if !self.control_mode {
            return;
        }
        self.pointer.check();
        if let Some(reason) = self.keyboard.revoked.or(self.pointer.revoked) {
            self.keyboard.revoke(reason);
            self.pointer.revoke(reason);
            self.control_events.clear();
            return;
        }
        self.control_events.extend(self.keyboard.take_events());
        let pointer_events = self.pointer.take_events();
        if pointer_events.iter().any(|event| {
            matches!(
                event,
                ViewerEvent::WaylandPointerCancel | ViewerEvent::WaylandPointerButtonCancel { .. }
            )
        }) {
            // Layout/new-frame cancellation also clears already collected pointer
            // edges. Keyboard dispatch order is retained.
            self.control_events
                .retain(|event| matches!(event, ViewerEvent::Key { .. }));
        }
        self.control_events.extend(pointer_events);
        if self.control_events.len() > 64 {
            self.keyboard.revoke("Wayland control event bound exceeded");
            self.pointer.revoke("Wayland control event bound exceeded");
            self.control_events.clear();
        }
    }
}
struct Retirement {
    id: u64,
    bytes: usize,
    started: Instant,
}
struct State {
    #[cfg(all(test, feature = "backend-wayland-udev"))]
    popup_live: std::sync::Arc<popup_presentation::Live>,
    control_mode: bool,
    control_events: Vec<ViewerEvent>,
    pointer: pointer::Pointer,
    keyboard: keyboard::Keyboard,
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
        #[cfg(all(test, feature = "backend-wayland-udev"))]
        self.popup_live.invalidate();
        self.pointer.revoke("Wayland pointer geometry changed");
        if self.keyboard.requested {
            self.keyboard.revoke("Wayland viewer geometry changed");
        }
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
    #[cfg(all(test, feature = "backend-wayland-udev"))]
    popup_presentation: popup_presentation::Bridge,
    registry: wl_registry::WlRegistry,
    conn: Connection,
    queue: EventQueue<State>,
    state: State,
    surface: wl_surface::WlSurface,
    _xdg_surface: xdg_surface::XdgSurface,
    toplevel: xdg_toplevel::XdgToplevel,
    base_title: String,
    pointer_title_status: Option<&'static str>,
    token: u64,
    slot_identity: u64,
    upload_scratch: Box<[u8]>,
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
        let registry = conn.display().get_registry(&qh, ());
        let mut state = State {
            #[cfg(all(test, feature = "backend-wayland-udev"))]
            popup_live: std::sync::Arc::new(popup_presentation::Live::default()),
            control_mode: false,
            control_events: Vec::new(),
            pointer: pointer::Pointer::default(),
            keyboard: keyboard::Keyboard::default(),
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
            registry,
            conn,
            queue,
            state,
            surface,
            _xdg_surface: xdg_surface,
            toplevel,
            base_title: title.to_owned(),
            pointer_title_status: None,
            token: 0,
            slot_identity: 0,
            upload_scratch: vec![0; MAX_UPLOAD_SCRATCH_BYTES].into_boxed_slice(),
            write_pending: std::cell::Cell::new(false),
            #[cfg(all(test, feature = "backend-wayland-udev"))]
            popup_presentation: popup_presentation::Bridge::default(),
        })
    }
    fn pump(&mut self) -> RemoteResult<bool> {
        let pumped = pump_connection(&self.conn, &mut self.queue, &mut self.state);
        #[cfg(all(test, feature = "backend-wayland-udev"))]
        if pumped.is_err() || self.state.closed {
            self.state.popup_live.end();
        }
        let work = pumped?;
        self.state.control_collect();
        #[cfg(all(test, feature = "backend-wayland-udev"))]
        CONTROL_SCROLL_OBSERVER.with(|slot| {
            if let Some(observer) = slot.borrow().as_ref() {
                observer.store(
                    self.state.pointer.scroll_frames_observed,
                    std::sync::atomic::Ordering::Release,
                );
            }
        });
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
    fn keyboard_fence(&mut self, deadline: Instant) -> RemoteResult<()> {
        let id = self
            .state
            .keyboard
            .fence
            .checked_add(1)
            .ok_or("keyboard fence exhausted")?;
        self.state.keyboard.fence = id;
        self.conn
            .display()
            .sync(&self.queue.handle(), keyboard::KeyboardFence(id));
        while self.state.keyboard.fence_done != id {
            self.pump()?;
            self.state.keyboard.check()?;
            if self.state.closed || Instant::now() >= deadline {
                self.state
                    .keyboard
                    .revoke("Wayland keyboard fence closed or expired");
                return self.state.keyboard.check();
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        Ok(())
    }
    pub(super) fn enable_wayland_control(
        &mut self,
        scroll_enabled: bool,
        horizontal_enabled: bool,
    ) -> RemoteResult<[u8; 32]> {
        if self.state.keyboard.requested || self.state.pointer.requested || self.state.control_mode
        {
            return Err(
                "Wayland control cannot be re-enabled or combined with an input mode".into(),
            );
        }
        let deadline = Instant::now() + TIMEOUT;
        self.keyboard_fence(deadline)?;
        if self.state.keyboard.ambiguous || self.state.keyboard.seats.len() != 1 {
            return Err("Wayland control requires exactly one unambiguous seat".into());
        }
        let (name, version) = self.state.keyboard.seats[0];
        if version < 5 {
            return Err("Wayland control requires a frame-capable seat".into());
        }
        self.state.control_mode = true;
        self.state.pointer.scroll_enabled = scroll_enabled;
        self.state.pointer.horizontal_enabled = horizontal_enabled;
        self.state.keyboard.requested = true;
        self.state.pointer.requested = true;
        self.state.keyboard.surface = Some(self.surface.clone());
        self.state.pointer.surface = Some(self.surface.clone());
        let seat = self
            .registry
            .bind::<wayland_client::protocol::wl_seat::WlSeat, _, _>(
                name,
                version.min(7),
                &self.queue.handle(),
                (),
            );
        self.state.keyboard.seat = Some(seat.clone());
        self.state.pointer.seat = Some(seat);
        self.toplevel.set_app_id("jwm-remote-control".into());
        loop {
            self.pump()?;
            self.state.keyboard.check()?;
            if let Some(digest) = self.state.keyboard.digest {
                if self.state.pointer.device.is_some() {
                    return Ok(digest);
                }
            }
            if self.state.closed || Instant::now() >= deadline {
                self.disable_wayland_control();
                return Err("Wayland control initialization expired".into());
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    pub(super) fn check_wayland_control(&mut self, initial: bool) -> RemoteResult<()> {
        if !self.state.control_mode {
            return Err("Wayland control is disabled".into());
        }
        self.keyboard_fence(Instant::now() + TIMEOUT)?;
        self.state.control_collect();
        self.state.keyboard.check()?;
        if initial
            && (!self.state.keyboard.clean_ready()
                || !self.state.pointer.locally_ready()
                || self.state.control_events.iter().any(|event| {
                    matches!(
                        event,
                        ViewerEvent::Key { .. }
                            | ViewerEvent::WaylandPointer { .. }
                            | ViewerEvent::WaylandPointerButtonCancel { pressed: true }
                    )
                })
                || self.state.pointer.control_busy())
        {
            self.disable_wayland_control();
            return Err("Wayland control requires clean dual focus and actual presentation".into());
        }
        Ok(())
    }
    pub(super) fn disable_wayland_control(&mut self) {
        self.state
            .keyboard
            .revoke("Wayland control permanently disabled");
        self.state
            .pointer
            .revoke("Wayland control permanently disabled");
        self.state.control_events.clear();
    }
    pub(super) fn enable_wayland_keyboard(
        &mut self,
        _display: Option<&str>,
    ) -> RemoteResult<[u8; 32]> {
        if self.state.keyboard.requested || self.state.pointer.requested {
            return Err("Wayland keyboard cannot be re-enabled or combined with pointer".into());
        }
        // Enumerate without binding any seat in the default read-only path.
        let deadline = Instant::now() + TIMEOUT;
        self.keyboard_fence(deadline)?;
        if self.state.keyboard.ambiguous || self.state.keyboard.seats.len() != 1 {
            return Err("Wayland keyboard requires exactly one unambiguous seat".into());
        }
        let (name, version) = self.state.keyboard.seats[0];
        if version < 4 {
            return Err("Wayland keyboard requires repeat-info capable seat".into());
        }
        self.state.keyboard.requested = true;
        self.toplevel.set_app_id("jwm-remote-keyboard".into());
        self.state.keyboard.surface = Some(self.surface.clone());
        self.state.keyboard.seat = Some(
            self.registry
                .bind::<wayland_client::protocol::wl_seat::WlSeat, _, _>(
                    name,
                    version.min(7),
                    &self.queue.handle(),
                    (),
                ),
        );
        loop {
            self.pump()?;
            self.state.keyboard.check()?;
            if self.state.closed || Instant::now() >= deadline {
                self.state
                    .keyboard
                    .revoke("Wayland keyboard initialization expired");
                return Err("Wayland keyboard initialization expired".into());
            }
            if let Some(digest) = self.state.keyboard.digest {
                return Ok(digest);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    pub(super) fn check_wayland_keyboard(&mut self, initial: bool) -> RemoteResult<()> {
        if !self.state.keyboard.requested {
            return Err("Wayland keyboard is disabled".into());
        }
        let deadline = Instant::now() + TIMEOUT;
        self.keyboard_fence(deadline)?;
        if initial {
            if self.state.keyboard.pending_keys() {
                self.state
                    .keyboard
                    .revoke("local keyboard input preceded lease activation");
                return self.state.keyboard.check();
            }
            while !self.state.keyboard.clean_ready() {
                self.pump()?;
                self.state.keyboard.check()?;
                if self.state.keyboard.pending_keys() {
                    self.state
                        .keyboard
                        .revoke("local keyboard input preceded lease activation");
                    return self.state.keyboard.check();
                }
                if self.state.closed || Instant::now() >= deadline {
                    self.state
                        .keyboard
                        .revoke("Wayland keyboard did not acquire clean own-surface focus");
                    return self.state.keyboard.check();
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        self.state.keyboard.check()
    }
    pub(super) fn disable_wayland_keyboard(&mut self) {
        self.state
            .keyboard
            .revoke("Wayland keyboard permanently disabled");
    }
    pub(super) fn enable_wayland_pointer(
        &mut self,
        scroll_enabled: bool,
        horizontal_enabled: bool,
    ) -> RemoteResult<()> {
        if self.state.pointer.requested || self.state.keyboard.requested {
            return Err("Wayland pointer cannot be re-enabled or combined with keyboard".into());
        }
        let deadline = Instant::now() + TIMEOUT;
        self.keyboard_fence(deadline)?; // Enumeration fence only; binds no keyboard.
        if self.state.keyboard.ambiguous || self.state.keyboard.seats.len() != 1 {
            return Err("Wayland pointer requires exactly one seat".into());
        }
        let (name, version) = self.state.keyboard.seats[0];
        if version < 5 {
            return Err("Wayland pointer requires frame-capable seat".into());
        }
        self.state.pointer.requested = true;
        self.state.pointer.scroll_enabled = scroll_enabled;
        self.state.pointer.horizontal_enabled = horizontal_enabled;
        self.state.pointer.surface = Some(self.surface.clone());
        self.toplevel.set_app_id("jwm-remote-pointer".into());
        self.state.pointer.seat = Some(self.registry.bind(
            name,
            version.min(7),
            &self.queue.handle(),
            pointer::PointerSeat,
        ));
        loop {
            self.pump()?;
            if let Some(reason) = self.state.pointer.revoked {
                return Err(reason.into());
            }
            if self.state.closed || Instant::now() >= deadline {
                self.state
                    .pointer
                    .revoke("Wayland pointer initialization expired");
                return Err("Wayland pointer initialization expired".into());
            }
            if self.state.pointer.device.is_some() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    pub(super) fn wayland_pointer_ready(&mut self, sequence: Option<u64>) -> RemoteResult<()> {
        if !self.state.pointer.requested
            || !(self.state.pointer.scroll_enabled || self.state.pointer.horizontal_enabled)
            || self.state.pointer.revoked.is_some()
            || self.state.pointer.ready() == sequence
        {
            return Ok(());
        }
        // A rising Ready gate must not promote scroll already queued by the
        // compositor while Waiting. Keep admission closed through a real sync.
        if self.state.control_mode {
            // These edges may already have left Pointer.completed for the
            // ordered combined queue. A new gate cannot authorize old scroll.
            self.state.control_events.retain(|event| {
                !matches!(
                    event,
                    ViewerEvent::WaylandPointer {
                        kind: super::x11_viewer::WaylandPointerEventKind::ScrollUp
                            | super::x11_viewer::WaylandPointerEventKind::ScrollDown
                            | super::x11_viewer::WaylandPointerEventKind::ScrollLeft
                            | super::x11_viewer::WaylandPointerEventKind::ScrollRight,
                        ..
                    }
                )
            });
        }
        self.state.pointer.set_ready(None);
        if sequence.is_none() || self.state.pointer.revoked.is_some() {
            return Ok(());
        }
        let snapshot = (
            self.state.generation,
            self.viewport(),
            self.state.scope,
            self.state.pointer.mapping(),
        );
        self.fence_pointer_events_inner(true)?;
        if self.state.pointer.revoked.is_none()
            && !self.state.closed
            && snapshot
                == (
                    self.state.generation,
                    self.viewport(),
                    self.state.scope,
                    self.state.pointer.mapping(),
                )
            && snapshot.3.is_some_and(|map| Some(map.sequence) == sequence)
            && self.state.pointer.locally_ready()
        {
            self.state.pointer.set_ready(sequence);
        }
        Ok(())
    }
    #[cfg(all(test, feature = "backend-wayland-udev"))]
    pub(crate) fn scroll_frames_observed_for_test(&self) -> u64 {
        self.state.pointer.scroll_frames_observed
    }
    /// Snapshot after normal event polling; no dispatch, focus change or release.
    pub(super) fn wayland_pointer_repair_busy(&self) -> bool {
        self.state.pointer.repair_busy()
    }
    /// Read-only local state after polling. Pending releases remain queued and
    /// unfinished device frames retain their original timeout.
    pub(super) fn wayland_control_repair_busy(&self) -> bool {
        self.state.control_mode
            && (self.state.closed
                || self.state.failure.is_some()
                || control_repair_busy(
                    &self.state.keyboard,
                    &self.state.pointer,
                    &self.state.control_events,
                ))
    }
    pub(super) fn wayland_pointer_presented(&mut self, sequence: u64) {
        self.state.pointer.confirm(sequence);
    }
    /// A host Ready is not local scroll readiness until the event-history fence
    /// completed for this exact currently presented mapping.
    pub(super) fn local_scroll_ready(&self, sequence: u64) -> bool {
        !(self.state.pointer.scroll_enabled || self.state.pointer.horizontal_enabled)
            || (self.state.pointer.ready() == Some(sequence)
                && self
                    .state
                    .pointer
                    .mapping()
                    .is_some_and(|map| map.sequence == sequence)
                && self.state.pointer.locally_ready())
    }
    pub(super) fn set_wayland_pointer_status(&mut self, status: &'static str) {
        let status = if self.state.pointer.revoked.is_some() {
            "Pointer: Revoked"
        } else if !self.state.pointer.locally_ready() {
            "Pointer: Waiting for local focus/presentation"
        } else if (self.state.pointer.scroll_enabled || self.state.pointer.horizontal_enabled)
            && !self
                .state
                .pointer
                .mapping()
                .is_some_and(|map| self.local_scroll_ready(map.sequence))
        {
            "Pointer: Waiting for local scroll synchronization"
        } else {
            status
        };
        if pointer_title_status_changed(&mut self.pointer_title_status, status) {
            self.toplevel
                .set_title(format!("{} — {status}", self.base_title));
        }
    }
    fn fence_pointer_events(&mut self) -> RemoteResult<()> {
        self.fence_pointer_events_inner(false)
    }
    fn fence_pointer_events_inner(&mut self, force: bool) -> RemoteResult<()> {
        if !force && !self.state.pointer.needs_fence() {
            return Ok(());
        }
        #[cfg(all(test, feature = "backend-wayland-udev"))]
        if !force {
            run_pointer_fence_hook();
        }
        let Some(id) = self.state.pointer.begin_fence() else {
            return Ok(());
        };
        self.conn
            .display()
            .sync(&self.queue.handle(), pointer::PointerFence(id));
        let deadline = Instant::now() + TIMEOUT;
        while self.state.pointer.fence_done != id {
            if let Err(error) = self.pump() {
                self.state
                    .pointer
                    .revoke("Wayland pointer history fence failed");
                return Err(error);
            }
            self.state.pointer.check();
            if self.state.closed || Instant::now() >= deadline {
                self.state
                    .pointer
                    .revoke("Wayland pointer history fence closed or expired");
                return Ok(());
            }
            if self.state.pointer.revoked.is_some() {
                return Ok(());
            }
            if self.state.pointer.fence_done != id {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        Ok(())
    }
    pub(super) fn poll_events(&mut self) -> RemoteResult<Vec<ViewerEvent>> {
        self.pump()?;
        // An input frame does not fence xdg/registry history. Synchronize the
        // same connection before releasing this batch, never future events.
        if self.state.control_mode {
            self.fence_pointer_events_inner(true)?;
            self.state.control_collect();
            self.state.keyboard.check()?;
            if self.state.closed {
                return Ok(vec![ViewerEvent::Close]);
            }
            return Ok(std::mem::take(&mut self.state.control_events));
        }
        self.fence_pointer_events()?;
        let mut events = self.state.keyboard.take_events();
        events.extend(self.state.pointer.take_events());
        if self.state.closed {
            events.clear();
            events.push(ViewerEvent::Close);
        }
        Ok(events)
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
        #[cfg(all(test, feature = "backend-wayland-udev"))]
        self.state.popup_live.invalidate();
        #[cfg(all(test, feature = "backend-wayland-udev"))]
        if self.popup_presentation.occupied() {
            return Ok(false);
        }
        self.pump()?;
        if !self.can_submit_frame() {
            return Ok(false);
        }
        self.state.pointer.invalidate_mapping();
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
        if slot.meta.busy {
            return Err("refusing to overwrite busy Wayland SHM storage".into());
        }
        slot.file.seek(SeekFrom::Start(0))?;
        upload_xrgb(
            &mut slot.file,
            image,
            width,
            height,
            &mut self.upload_scratch,
        )?;
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
            mapping: pointer::Mapping {
                sequence: frame.sequence(),
                source_epoch: scope.0,
                geometry_epoch: scope.1,
                generation: self.state.generation,
                viewport: (width, height),
                encoded: (frame.image().width() as u16, frame.image().height() as u16),
                source: (frame.source_width(), frame.source_height()),
            },
            token: self.token,
            sequence: frame.sequence(),
            source_epoch: scope.0,
            geometry_epoch: scope.1,
            generation: self.state.generation,
            presented: false,
            started: Instant::now(),
        });
        // Test capture observers can hold this same lock while dispatching and
        // reading back the owned compositor. Acquire before emitting commit,
        // so its sequence cannot race ahead of the pixels being inspected.
        #[cfg(all(test, feature = "backend-wayland-udev"))]
        let commit_observer = CONTROL_FRAME_OBSERVER.with(|slot| slot.borrow().clone());
        #[cfg(all(test, feature = "backend-wayland-udev"))]
        let mut commit_observation = commit_observer.as_ref().map(|o| o.lock().unwrap());
        self.surface.attach(Some(&buffer), 0, 0);
        self.surface
            .damage_buffer(0, 0, i32::from(width), i32::from(height));
        self.surface.commit();
        self.prepare_wait()?;
        #[cfg(all(test, feature = "backend-wayland-udev"))]
        if let Some(observation) = commit_observation.as_mut() {
            observation.0 = Some(frame.sequence());
        }
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
        let sequences = std::mem::take(&mut self.state.completed);
        #[cfg(all(test, feature = "backend-wayland-udev"))]
        PRESENTED_TIME_OBSERVER.with(|slot| {
            if let Some(observer) = slot.borrow().as_ref() {
                let drained_at = Instant::now();
                let mut ledger = observer.lock().unwrap();
                assert!(
                    ledger.len().saturating_add(sequences.len()) <= 4096,
                    "test presentation time ledger exhausted"
                );
                ledger.extend(
                    sequences
                        .iter()
                        .copied()
                        .map(|sequence| (sequence, drained_at)),
                );
            }
        });
        #[cfg(all(test, feature = "backend-wayland-udev"))]
        if let Some(sequence) = sequences.last().copied() {
            CONTROL_FRAME_OBSERVER.with(|slot| {
                if let Some(observer) = slot.borrow().as_ref() {
                    observer.lock().unwrap().1 = Some(sequence);
                }
            });
        }
        #[cfg(all(test, feature = "backend-wayland-udev"))]
        self.popup_presentation.observe_presented(
            &sequences,
            self.token,
            self.state.generation,
            self.state.scope,
        )?;
        Ok(sequences)
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
        Ok(work
            || self.state.closed
            || !self.state.completed.is_empty()
            || self.state.pointer.has_events())
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
        let pending = match (pending, self.state.pointer.deadline()) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
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
        self.state.pointer.revoke("Wayland pointer source changed");
        self.state.failure =
            Some("Wayland viewer source switching is unsupported; session closed".into());
        self.state.completed.clear();
        self.state.check()
    }
    pub(super) fn source_presented(&mut self, _epoch: u64) {}
    pub(super) fn set_source_label(&mut self, label: String) {
        let label: String = label
            .chars()
            .filter(|c| !c.is_control())
            .take(256)
            .collect();
        if label != self.base_title {
            self.base_title = label;
            self.pointer_title_status = None;
            self.toplevel.set_title(self.base_title.clone());
        }
    }
}
fn pointer_title_status_changed(cached: &mut Option<&'static str>, status: &'static str) -> bool {
    if *cached == Some(status) {
        return false;
    }
    *cached = Some(status);
    true
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
                    "wl_seat" => {
                        state
                            .pointer
                            .revoke("Wayland pointer seat topology changed");
                        state.keyboard.global(name, version);
                        return;
                    }
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
            wl_registry::Event::GlobalRemove { name } => {
                if state.keyboard.seats.iter().any(|(id, _)| *id == name) {
                    state.pointer.revoke("Wayland pointer seat removed");
                }
                state.keyboard.removed(name);
                if state.globals.contains(&name) {
                    state.failure = Some("required Wayland global removed".into());
                }
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
            xdg_toplevel::Event::Configure {
                width,
                height,
                states,
            } => {
                #[cfg(all(test, feature = "backend-wayland-udev"))]
                state.popup_live.invalidate();
                state.pointer.activation(&states);
                if state.pointer.requested
                    && state.configured
                    && ((width != 0 && width != i32::from(state.width))
                        || (height != 0 && height != i32::from(state.height)))
                {
                    state
                        .pointer
                        .revoke("Wayland pointer resize history changed");
                }
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
            xdg_toplevel::Event::Close => {
                #[cfg(all(test, feature = "backend-wayland-udev"))]
                state.popup_live.end();
                state.closed = true;
            }
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
                    state.pointer.presented(p.mapping);
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
    fn control_repair_busy_preserves_collected_release_and_revocation() {
        let mut keyboard = keyboard::Keyboard::default();
        let pointer = pointer::Pointer::default();
        assert!(!control_repair_busy(&keyboard, &pointer, &[]));
        let events = vec![ViewerEvent::Key {
            keycode: 50,
            pressed: false,
        }];
        for _ in 0..2 {
            assert!(control_repair_busy(&keyboard, &pointer, &events));
        }
        assert_eq!(
            events,
            vec![ViewerEvent::Key {
                keycode: 50,
                pressed: false
            }]
        );
        keyboard.revoke("test local revocation");
        assert!(control_repair_busy(&keyboard, &pointer, &[]));
        assert!(control_repair_busy(&keyboard, &pointer, &[]));
    }

    #[test]
    fn pointer_title_status_cache_only_updates_for_final_display_changes() {
        let mut cached = None;
        assert!(pointer_title_status_changed(
            &mut cached,
            "Pointer: Waiting for local focus/presentation"
        ));
        for _ in 0..100 {
            assert!(!pointer_title_status_changed(
                &mut cached,
                "Pointer: Waiting for local focus/presentation"
            ));
        }
        assert!(pointer_title_status_changed(&mut cached, "Pointer: Ready"));
        assert!(!pointer_title_status_changed(&mut cached, "Pointer: Ready"));
        assert!(pointer_title_status_changed(
            &mut cached,
            "Pointer: Revoked"
        ));
        cached = None; // A changed source label requires one refreshed title.
        assert!(pointer_title_status_changed(
            &mut cached,
            "Pointer: Revoked"
        ));
    }

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

#[cfg(test)]
mod upload_tests {
    use super::*;
    use image::{Rgb, RgbImage};

    // Keep the pre-optimization row algorithm as an independent byte oracle.
    fn reference(image: &RgbImage, width: u16, height: u16) -> Vec<u8> {
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
        let mut bytes = Vec::new();
        let mut row = vec![0; usize::from(width) * 4];
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
            bytes.extend_from_slice(&row);
        }
        bytes
    }
    #[derive(Default)]
    struct Writer {
        bytes: Vec<u8>,
        calls: usize,
        limit: Option<usize>,
        interrupt_at: Option<usize>,
        fail_at: Option<(usize, io::ErrorKind)>,
    }
    impl Write for Writer {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.calls += 1;
            if self.interrupt_at == Some(self.calls) {
                return Err(io::ErrorKind::Interrupted.into());
            }
            if let Some((call, kind)) = self.fail_at
                && call == self.calls
            {
                if kind == io::ErrorKind::WriteZero {
                    return Ok(0);
                }
                return Err(kind.into());
            }
            let count = self.limit.unwrap_or(bytes.len()).min(bytes.len());
            self.bytes.extend_from_slice(&bytes[..count]);
            Ok(count)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    fn image(width: u32, height: u32) -> RgbImage {
        RgbImage::from_fn(width, height, |x, y| {
            Rgb([(x * 13 + y) as u8, (y * 17 + x) as u8, (x ^ y) as u8])
        })
    }
    #[test]
    fn chunk_upload_matches_old_rows_for_odd_scaling_black_bars_and_reused_scratch() {
        let mut scratch = vec![0xa5; MAX_UPLOAD_SCRATCH_BYTES];
        for (iw, ih, width, height) in [
            (1, 1, 1, 1),
            (3, 7, 13, 9),
            (7, 3, 9, 13),
            (13, 11, 7, 5),
            (1, 37, 15, 11),
            (37, 1, 11, 15),
            (17, 13, 641, 479),
        ] {
            let image = image(iw, ih);
            let mut writer = Writer::default();
            upload_xrgb(&mut writer, &image, width, height, &mut scratch).unwrap();
            assert_eq!(writer.bytes, reference(&image, width, height));
            assert_eq!(
                writer.bytes.len(),
                usize::from(width) * usize::from(height) * 4
            );
        }
    }
    #[test]
    fn counting_writer_observes_chunk_calls_instead_of_rows_and_no_tail_garbage() {
        let image = image(17, 13);
        let mut scratch = vec![0xff; MAX_UPLOAD_SCRATCH_BYTES];
        let mut writer = Writer::default();
        upload_xrgb(&mut writer, &image, 640, 480, &mut scratch).unwrap();
        assert_eq!(writer.calls, 5);
        assert_eq!(writer.bytes, reference(&image, 640, 480));
        let mut tiny = vec![0xff; 17 * 4 * 2 + 3];
        let mut writer = Writer::default();
        upload_xrgb(&mut writer, &image, 17, 19, &mut tiny).unwrap();
        assert_eq!(writer.calls, 10);
        assert_eq!(writer.bytes, reference(&image, 17, 19));
        // Maximum legal width gives a 64 KiB row: four rows per full
        // scratch, and an exact one-row tail must not expose stale bytes.
        let mut writer = Writer::default();
        upload_xrgb(&mut writer, &image, 16384, 9, &mut scratch).unwrap();
        assert_eq!(writer.calls, 3);
        assert_eq!(writer.bytes, reference(&image, 16384, 9));
        // A non-integral scratch boundary must round DOWN to complete rows.
        let mut writer = Writer::default();
        upload_xrgb(
            &mut writer,
            &image,
            16384,
            9,
            &mut scratch[..MAX_UPLOAD_SCRATCH_BYTES - 1],
        )
        .unwrap();
        assert_eq!(writer.calls, 3);
        assert_eq!(writer.bytes, reference(&image, 16384, 9));
    }
    #[test]
    fn short_writes_and_interrupted_preserve_exact_pixels() {
        let image = image(3, 7);
        let mut scratch = vec![0; 13 * 4 * 2];
        let mut writer = Writer {
            limit: Some(3),
            interrupt_at: Some(2),
            ..Writer::default()
        };
        upload_xrgb(&mut writer, &image, 13, 9, &mut scratch).unwrap();
        assert_eq!(writer.bytes, reference(&image, 13, 9));
    }
    #[test]
    fn write_zero_and_mid_upload_error_stop_without_further_writes() {
        let image = image(3, 7);
        let mut scratch = vec![0; 13 * 4 * 2];
        for kind in [io::ErrorKind::WriteZero, io::ErrorKind::Other] {
            let mut writer = Writer {
                fail_at: Some((2, kind)),
                ..Writer::default()
            };
            assert_eq!(
                upload_xrgb(&mut writer, &image, 13, 9, &mut scratch)
                    .unwrap_err()
                    .kind(),
                kind
            );
            assert_eq!(writer.calls, 2);
            assert_eq!(writer.bytes, reference(&image, 13, 9)[..104]);
        }
    }
    #[test]
    fn invalid_scratch_or_dimensions_write_nothing() {
        for length in [0, 13 * 4 - 1, MAX_UPLOAD_SCRATCH_BYTES + 1] {
            let mut scratch = vec![0; length];
            let mut writer = Writer::default();
            assert!(upload_xrgb(&mut writer, &image(3, 7), 13, 9, &mut scratch).is_err());
            assert_eq!(writer.calls, 0);
        }
        let mut scratch = vec![0; MAX_UPLOAD_SCRATCH_BYTES];
        let mut writer = Writer::default();
        assert!(upload_xrgb(&mut writer, &image(1, 1), 0, 1, &mut scratch).is_err());
        assert!(upload_xrgb(&mut writer, &RgbImage::new(0, 1), 1, 1, &mut scratch).is_err());
        assert_eq!(writer.calls, 0);
    }
}

#[cfg(test)]
#[path = "wayland_viewer_upload_benchmark.rs"]
mod upload_benchmark;

// A test may pause only after a real completed pointer frame exists, before
// its real protocol fence. This never supplies state, presentation or authority.
#[cfg(all(test, feature = "backend-wayland-udev"))]
thread_local! {
    static POINTER_FENCE_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const {
        std::cell::RefCell::new(None)
    };
}
#[cfg(all(test, feature = "backend-wayland-udev"))]
pub(crate) fn with_pointer_fence_hook<T>(
    hook: impl FnOnce() + 'static,
    run: impl FnOnce() -> T,
) -> T {
    struct Restore(Option<Box<dyn FnOnce()>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            POINTER_FENCE_HOOK.with(|slot| {
                slot.replace(self.0.take());
            });
        }
    }
    let _restore = Restore(POINTER_FENCE_HOOK.with(|slot| slot.replace(Some(Box::new(hook)))));
    run()
}
#[cfg(all(test, feature = "backend-wayland-udev"))]
fn run_pointer_fence_hook() {
    let hook = POINTER_FENCE_HOOK.with(|slot| slot.borrow_mut().take());
    if let Some(hook) = hook {
        hook();
    }
}
