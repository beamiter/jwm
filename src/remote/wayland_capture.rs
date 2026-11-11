//! One local, interactive Wayland toplevel grant, retained on one connection.
//!
//! No X11 access, output fallback, identifier lookup, reconnection, cursor,
//! input, clipboard, or native resize. Protocol globals are not a per-client
//! permission boundary: this host must itself be a trusted local client.
#[path = "wayland_capture_control.rs"]
mod control;
#[path = "wayland_capture_popup.rs"]
mod interactive_popup;
#[path = "wayland_capture_text.rs"]
mod text;
use super::RemoteResult;
use super::messages::FrameLimits;
use super::x11_capture::{CaptureImageRecycler, CaptureOutcome, CapturedFrame, frame_dimensions};
use crate::readonly_popup_capture_protocol::client::jwm_readonly_popup_capture_manager_v1::{
    self as popup_manager, JwmReadonlyPopupCaptureManagerV1,
};
use crate::readonly_resize_capture_protocol::client::jwm_readonly_resize_capture_manager_v1::{
    self as resize_manager, JwmReadonlyResizeCaptureManagerV1,
};
use crate::remote_control_protocol::client::jwm_remote_control_lease_v1::JwmRemoteControlLeaseV1;
use crate::remote_keyboard_protocol::client::{
    jwm_remote_keyboard_lease_v1::{self as keyboard_lease, JwmRemoteKeyboardLeaseV1},
    jwm_remote_keyboard_manager_v1::JwmRemoteKeyboardManagerV1,
};
use crate::remote_pointer_protocol::client::{
    jwm_remote_pointer_lease_v1::{self as pointer_lease, JwmRemotePointerLeaseV1},
    jwm_remote_pointer_manager_v1::JwmRemotePointerManagerV1,
};
use control::NativeControl;
use image::{RgbImage, imageops::FilterType};
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::memfd::{MFdFlags, memfd_create};
use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::fs::FileExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use wayland_client::backend::WaylandError;
use wayland_client::protocol::{
    wl_buffer, wl_callback, wl_output, wl_registry, wl_shm, wl_shm_pool,
};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::{
    ext_foreign_toplevel_handle_v1::{self as handle, ExtForeignToplevelHandleV1},
    ext_foreign_toplevel_list_v1::{self as list, ExtForeignToplevelListV1},
};
use wayland_protocols::ext::image_capture_source::v1::client::{
    ext_foreign_toplevel_image_capture_source_manager_v1::ExtForeignToplevelImageCaptureSourceManagerV1,
    ext_image_capture_source_v1::ExtImageCaptureSourceV1,
};
use wayland_protocols::ext::image_copy_capture::v1::client::{
    ext_image_copy_capture_frame_v1::{self as frame, ExtImageCopyCaptureFrameV1},
    ext_image_copy_capture_manager_v1::{self as manager, ExtImageCopyCaptureManagerV1},
    ext_image_copy_capture_session_v1::{self as session, ExtImageCopyCaptureSessionV1},
};

#[cfg(all(test, feature = "backend-wayland-udev"))]
type CaptureFrameObservation = std::sync::Arc<std::sync::Mutex<Vec<(u32, u32, u64)>>>;
#[cfg(all(test, feature = "backend-wayland-udev"))]
thread_local! { static CAPTURE_FRAME_OBSERVER: std::cell::RefCell<Option<CaptureFrameObservation>> = const { std::cell::RefCell::new(None) }; }
#[cfg(all(test, feature = "backend-wayland-udev"))]
pub(crate) fn with_wayland_capture_frame_observer<T>(
    observer: CaptureFrameObservation,
    run: impl FnOnce() -> T,
) -> T {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            CAPTURE_FRAME_OBSERVER.with(|slot| {
                slot.borrow_mut().take();
            });
        }
    }
    CAPTURE_FRAME_OBSERVER.with(|slot| {
        assert!(slot.borrow().is_none());
        *slot.borrow_mut() = Some(observer);
    });
    let _reset = Reset;
    run()
}
const PROTOCOL_TIMEOUT: Duration = Duration::from_secs(5);
const SELECTION_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_TOPLEVELS: usize = 256;
const MAX_LABEL_BYTES: usize = 256;
const MAX_SELECTION_BYTES: usize = 32;

struct Toplevel {
    handle: ExtForeignToplevelHandleV1,
    title: String,
    app_id: String,
    complete: bool,
    closed: bool,
}

#[derive(Default)]
struct NativeKeyboard {
    manager: Option<JwmRemoteKeyboardManagerV1>,
    global: Option<u32>,
    enabled: bool,
    revoked: bool,
    requested: bool,
    digest: Option<[u8; 32]>,
    epoch: Option<u32>,
    armed: bool,
    arm_sent: bool,
    accepted: u32,
    expected: Option<u32>,
    deadline: Option<Instant>,
}

#[derive(Default)]
struct NativePointer {
    scroll_enabled: bool,
    horizontal_enabled: bool,
    manager: Option<JwmRemotePointerManagerV1>,
    global: Option<u32>,
    enabled: bool,
    revoked: bool,
    requested: bool,
    epoch: Option<u32>,
    accepted: u32,
    expected: Option<u32>,
    deadline: Option<Instant>,
}
#[derive(Default)]
struct State {
    interactive_popup: interactive_popup::NativePopup,
    popups: bool,
    popup_manager: Option<JwmReadonlyPopupCaptureManagerV1>,
    popup_layout: Option<(u16, u16)>,
    text: text::NativeText,
    resize_follow: bool,
    resize_manager: Option<JwmReadonlyResizeCaptureManagerV1>,
    session_stopped: bool,
    frame_failed: bool,
    follow_epoch: u32,
    follow_suspended: bool,
    follow_layout: Option<(u16, u16)>,
    control: NativeControl,
    pointer: NativePointer,
    keyboard: NativeKeyboard,
    shm: Option<wl_shm::WlShm>,
    list: Option<ExtForeignToplevelListV1>,
    source_manager: Option<ExtForeignToplevelImageCaptureSourceManagerV1>,
    capture_manager: Option<ExtImageCopyCaptureManagerV1>,
    globals: Vec<u32>,
    toplevels: Vec<Toplevel>,
    selected: Option<ExtForeignToplevelHandleV1>,
    failed: Option<&'static str>,
    sync_count: u64,
    shm_xrgb: bool,
    size: Option<(u16, u16)>,
    batch_size: Option<(u16, u16)>,
    batch_xrgb: bool,
    constraints_done: bool,
    frame_serial: u64,
    frame_ready: bool,
    normal_transform: bool,
}

impl State {
    fn follow_event(&mut self, event: resize_manager::Event) {
        if self.failed.is_some() {
            return;
        }
        match event {
            resize_manager::Event::Suspended { epoch } => {
                if epoch == 0 || epoch <= self.follow_epoch {
                    self.fail("nonmonotonic Wayland resize-follow suspension");
                    return;
                }
                self.follow_epoch = epoch;
                self.follow_suspended = true;
                self.follow_layout = None;
                self.session_stopped = true;
            }
            resize_manager::Event::Layout {
                epoch,
                width,
                height,
            } => {
                if !self.follow_suspended
                    || epoch != self.follow_epoch
                    || buffer_layout(width, height).is_err()
                {
                    self.fail("invalid or stale Wayland resize-follow layout");
                    return;
                }
                self.follow_epoch = epoch;
                self.follow_suspended = false;
                self.follow_layout = Some((width as u16, height as u16));
            }
            resize_manager::Event::Stopped => {
                self.fail("Wayland resize-follow source permanently stopped")
            }
        }
    }

    fn fail(&mut self, message: &'static str) {
        self.failed.get_or_insert(message);
    }
    fn check(&self) -> RemoteResult<()> {
        self.interactive_popup.check()?;
        if let Some(message) = self.failed {
            return Err(invalid(message).into());
        }
        if let Some(selected) = &self.selected {
            if !self
                .toplevels
                .iter()
                .any(|item| &item.handle == selected && item.complete && !item.closed)
            {
                return Err(invalid("selected Wayland toplevel is no longer live").into());
            }
        }
        Ok(())
    }
    fn advertise_size(&mut self, width: u32, height: u32) {
        self.begin_constraints();
        if buffer_layout(width, height).is_err() {
            self.fail("invalid Wayland source size");
            return;
        }
        let size = (width as u16, height as u16);
        if self.size.is_some_and(|original| original != size) {
            self.fail("Wayland source size changed; select again locally");
            return;
        }
        // Freeze even before Done, so initial resize/restore cannot replace
        // the grant while format constraints are still arriving.
        self.size = Some(size);
        self.batch_size = Some(size);
    }

    fn begin_constraints(&mut self) {
        if self.constraints_done {
            self.constraints_done = false;
            self.batch_size = None;
            self.batch_xrgb = false;
        }
    }
}

pub(super) struct WaylandCapture {
    conn: Connection,
    queue: EventQueue<State>,
    state: State,
    source: Option<ExtImageCaptureSourceV1>,
    session: Option<ExtImageCopyCaptureSessionV1>,
    inflight: Option<ExtImageCopyCaptureFrameV1>,
    buffer: Option<wl_buffer::WlBuffer>,
    file: Option<File>,
    host_width_limit: u16,
    max_width: u16,
    max_height: u16,
    limits: FrameLimits,
    epoch: u64,
    resize_sessions: u32,
    applied_follow_epoch: u32,
    recycler: CaptureImageRecycler,
    control_lease: Option<JwmRemoteControlLeaseV1>,
    pointer_lease: Option<JwmRemotePointerLeaseV1>,
    pointer_ready_at: Option<Instant>,
    keyboard_lease: Option<JwmRemoteKeyboardLeaseV1>,
    retained_frame: Option<ExtImageCopyCaptureFrameV1>,
    arm_deadline: Option<Instant>,
}

impl WaylandCapture {
    pub(super) fn connect(
        display: Option<&str>,
        max_width: u16,
        resize_follow: bool,
        popups: bool,
    ) -> RemoteResult<Self> {
        let path = socket_path(display)?;
        let stream = jwm_ipc_transport::connect(&path, PROTOCOL_TIMEOUT)?;
        Self::connect_stream(stream, max_width, resize_follow, popups, |capture| {
            eprint!("jwm-remote: select Wayland window index: ");
            io::stderr().flush()?;
            capture.read_selection(Instant::now() + SELECTION_TIMEOUT)
        })
    }

    /// Exercise the production protocol path against a private test server.
    /// Only local selection is replaced; index is one-based, as in the CLI.
    #[cfg(all(test, feature = "backend-wayland-udev"))]
    pub(crate) fn connect_test_stream(
        stream: UnixStream,
        index: usize,
        max_width: u16,
    ) -> RemoteResult<Self> {
        Self::connect_stream(stream, max_width, false, false, |_| {
            index
                .checked_sub(1)
                .filter(|index| *index < MAX_TOPLEVELS)
                .ok_or_else(|| invalid("invalid Wayland test selection index").into())
        })
    }

    #[cfg(all(test, feature = "backend-wayland-udev"))]
    pub(crate) fn connect_test_stream_follow(
        stream: UnixStream,
        index: usize,
        max_width: u16,
    ) -> RemoteResult<Self> {
        Self::connect_stream(stream, max_width, true, false, |_| {
            index
                .checked_sub(1)
                .filter(|index| *index < MAX_TOPLEVELS)
                .ok_or_else(|| invalid("invalid Wayland test selection index").into())
        })
    }
    #[cfg(all(test, feature = "backend-wayland-udev"))]
    pub(crate) fn connect_test_stream_popups(
        stream: UnixStream,
        index: usize,
        max_width: u16,
    ) -> RemoteResult<Self> {
        Self::connect_stream(stream, max_width, false, true, |_| {
            index
                .checked_sub(1)
                .filter(|index| *index < MAX_TOPLEVELS)
                .ok_or_else(|| invalid("invalid Wayland test selection index").into())
        })
    }
    #[cfg(all(test, feature = "backend-wayland-udev"))]
    pub(crate) fn connect_test_stream_popups_follow(
        stream: UnixStream,
        index: usize,
        max_width: u16,
    ) -> RemoteResult<Self> {
        Self::connect_stream(stream, max_width, true, true, |_| {
            index
                .checked_sub(1)
                .filter(|index| *index < MAX_TOPLEVELS)
                .ok_or_else(|| invalid("invalid Wayland test selection index").into())
        })
    }
    fn connect_stream(
        stream: UnixStream,
        max_width: u16,
        resize_follow: bool,
        popups: bool,
        select: impl FnOnce(&mut Self) -> RemoteResult<usize>,
    ) -> RemoteResult<Self> {
        Self::connect_stream_mode(stream, max_width, resize_follow, popups, false, select)
    }
    fn connect_stream_mode(
        stream: UnixStream,
        max_width: u16,
        resize_follow: bool,
        popups: bool,
        interactive_popup: bool,
        select: impl FnOnce(&mut Self) -> RemoteResult<usize>,
    ) -> RemoteResult<Self> {
        if interactive_popup && (resize_follow || popups || max_width != 0) {
            return Err(invalid("interactive family requires an unscaled exclusive root").into());
        }
        stream.set_nonblocking(true)?;
        let conn = Connection::from_socket(stream)?;
        let queue = conn.new_event_queue();
        let qh = queue.handle();
        conn.display().get_registry(&qh, ());
        let mut capture = Self {
            conn,
            queue,
            state: State {
                interactive_popup: interactive_popup::NativePopup::new(interactive_popup),
                resize_follow,
                popups,
                ..Default::default()
            },
            source: None,
            session: None,
            inflight: None,
            buffer: None,
            file: None,
            host_width_limit: max_width,
            max_width,
            max_height: 0,
            limits: FrameLimits::default(),
            epoch: 1,
            resize_sessions: 0,
            applied_follow_epoch: 0,
            recycler: CaptureImageRecycler::default(),
            control_lease: None,
            pointer_lease: None,
            pointer_ready_at: None,
            keyboard_lease: None,
            retained_frame: None,
            arm_deadline: None,
        };
        let deadline = Instant::now() + PROTOCOL_TIMEOUT;
        // First sync obtains globals. Binds issued by their callbacks precede
        // the second sync, which obtains initial handles and metadata batches.
        capture.sync_until(deadline)?;
        capture.sync_until(deadline)?;
        if capture.state.shm.is_none()
            || !capture.state.shm_xrgb
            || capture.state.list.is_none()
            || capture.state.source_manager.is_none()
            || capture.state.capture_manager.is_none()
        {
            return Err(invalid(
                "Wayland compositor lacks required toplevel, image-copy, or XRGB SHM support",
            )
            .into());
        }
        let mut listed = Vec::new();
        for (index, item) in capture.state.toplevels.iter().enumerate() {
            if item.complete && !item.closed {
                eprintln!(
                    "jwm-remote: Wayland window [{}]: title={} app_id={}",
                    index + 1,
                    item.title,
                    item.app_id
                );
                listed.push(index);
            }
        }
        if listed.is_empty() {
            return Err(invalid("no live Wayland toplevels are available").into());
        }
        let index = select(&mut capture)?;
        if !listed.contains(&index) {
            return Err(invalid("Wayland index was not in the displayed selection list").into());
        }
        let selected = capture
            .state
            .toplevels
            .get(index)
            .filter(|item| item.complete && !item.closed)
            .ok_or_else(|| invalid("selected Wayland index is not a live listed window"))?
            .handle
            .clone();
        capture.state.selected = Some(selected.clone());
        // A closed handle during local selection can never become an output,
        // a replacement identifier, or a second interactive selection.
        let deadline = Instant::now() + PROTOCOL_TIMEOUT;
        capture.sync_until(deadline)?;
        capture.state.check()?;
        let source = capture
            .state
            .source_manager
            .as_ref()
            .unwrap()
            .create_source(&selected, &qh, ());
        let source = if interactive_popup {
            capture.create_interactive_popup_source(source)?
        } else {
            source
        };
        let source = if popups && !resize_follow {
            let manager = capture.state.popup_manager.as_ref().ok_or_else(|| {
                invalid("Wayland compositor lacks explicit read-only popup capture capability")
            })?;
            let derived = manager.create_source(&source, &qh, ());
            source.destroy();
            capture.drive_until(deadline, |state| state.popup_layout.is_some())?;
            derived
        } else {
            source
        };
        let source = if resize_follow {
            let manager = capture.state.resize_manager.as_ref().ok_or_else(|| {
                invalid("Wayland compositor lacks explicit read-only resize-follow capability")
            })?;
            require_resize_capability(manager.version(), popups)?;
            let derived = if popups {
                manager.create_source_with_popups(&source, &qh, ())
            } else {
                manager.create_source(&source, &qh, ())
            };
            source.destroy();
            derived
        } else {
            source
        };
        if resize_follow {
            capture.drive_until(deadline, |state| {
                state.follow_layout.is_some() && !state.follow_suspended
            })?;
            capture.applied_follow_epoch = capture.state.follow_epoch;
            capture.state.session_stopped = false;
        }
        let session = capture
            .state
            .capture_manager
            .as_ref()
            .unwrap()
            .create_session(&source, manager::Options::empty(), &qh, ());
        capture.source = Some(source);
        capture.session = Some(session);
        capture.drive_until(deadline, |state| {
            state.constraints_done || state.session_stopped
        })?;
        if capture.state.session_stopped {
            return Err(
                invalid("Wayland source session stopped before initial constraints").into(),
            );
        }
        if resize_follow && capture.state.size != capture.state.follow_layout {
            return Err(
                invalid("Wayland resize-follow constraints disagree with native layout").into(),
            );
        }
        if popups && !resize_follow && capture.state.size != capture.state.popup_layout {
            return Err(
                invalid("Wayland popup constraints disagree with fixed root canvas").into(),
            );
        }
        capture.create_buffer()?;
        capture.sync_until(deadline)?;
        Ok(capture)
    }

    /// Must be enabled before the first frame and only after wire negotiation.
    pub(super) fn enable_pointer(&mut self) -> RemoteResult<()> {
        if self.state.interactive_popup.enabled
            || self.state.text.enabled
            || self.state.resize_follow
            || self.state.popups
            || self.state.frame_serial != 0
            || self.state.control.enabled
            || self.state.keyboard.enabled
            || self.state.pointer.enabled
            || self.state.pointer.manager.is_none()
        {
            return Err(invalid("native pointer unavailable or enable too late").into());
        }
        self.state.pointer.enabled = true;
        Ok(())
    }
    pub(super) fn enable_scroll(&mut self) -> RemoteResult<()> {
        if self.state.frame_serial != 0
            || !self.state.pointer.enabled
            || self.state.pointer.requested
            || self
                .state
                .pointer
                .manager
                .as_ref()
                .is_none_or(|manager| manager.version() < 2)
        {
            return Err(invalid("native scroll requires explicit pointer v2 permission").into());
        }
        self.state.pointer.scroll_enabled = true;
        Ok(())
    }
    pub(super) fn enable_horizontal_scroll(&mut self) -> RemoteResult<()> {
        if self.state.frame_serial != 0
            || !self.state.pointer.enabled
            || self.state.pointer.requested
            || self
                .state
                .pointer
                .manager
                .as_ref()
                .is_none_or(|manager| manager.version() < 3)
        {
            return Err(invalid(
                "native horizontal scroll requires explicit pointer v3 permission",
            )
            .into());
        }
        self.state.pointer.horizontal_enabled = true;
        Ok(())
    }
    pub(super) fn pointer_revoked(&self) -> bool {
        self.state.pointer.revoked
            || self
                .state
                .pointer
                .deadline
                .is_some_and(|at| Instant::now() >= at)
    }
    pub(super) fn pointer_frame(&self) -> Option<(u64, Instant)> {
        if !self.state.pointer.enabled || self.retained_frame.is_none() {
            return None;
        }
        self.pointer_ready_at
            .map(|at| (self.state.frame_serial, at))
    }
    pub(super) fn discard_pointer_frame(&mut self) {
        if self.state.pointer.enabled {
            if let Some(frame) = self.retained_frame.take() {
                frame.destroy();
            }
            self.pointer_ready_at = None;
        }
    }
    pub(super) fn revoke_pointer(&mut self) {
        self.state.pointer.revoked = true;
        if let Some(lease) = self.pointer_lease.take() {
            lease.destroy();
        }
        self.discard_pointer_frame();
        let _ = self.conn.flush();
    }
    pub(super) fn grant_pointer(&mut self) -> RemoteResult<u64> {
        if !self.state.pointer.enabled
            || self.pointer_revoked()
            || self.state.pointer.requested
            || self.retained_frame.is_none()
        {
            return Err(invalid("native pointer grant not permitted").into());
        }
        self.state.pointer.requested = true;
        self.state.pointer.deadline =
            Some(Instant::now() + super::wayland_pointer_wire::LEASE_DURATION);
        let manager = self.state.pointer.manager.as_ref().unwrap();
        self.pointer_lease = Some(if self.state.pointer.horizontal_enabled {
            manager.create_lease_with_scroll_axes(
                self.source.as_ref().unwrap(),
                2 | u32::from(self.state.pointer.scroll_enabled),
                &self.queue.handle(),
                (),
            )
        } else if self.state.pointer.scroll_enabled {
            manager.create_lease_with_scroll(
                self.source.as_ref().unwrap(),
                &self.queue.handle(),
                (),
            )
        } else {
            manager.create_lease(self.source.as_ref().unwrap(), &self.queue.handle(), ())
        });
        let deadline = Instant::now() + PROTOCOL_TIMEOUT;
        loop {
            self.state.check()?;
            if self.pointer_revoked() {
                self.revoke_pointer();
                return Err(invalid("native pointer revoked").into());
            }
            if let Some(epoch) = self.state.pointer.epoch {
                return Ok(u64::from(epoch));
            }
            if Instant::now() >= deadline {
                self.revoke_pointer();
                return Err(invalid("native pointer grant timed out").into());
            }
            self.pump(deadline)?;
        }
    }
    pub(super) fn pointer_action(
        &mut self,
        action: &super::wayland_pointer_wire::PointerAction,
        serial: u64,
    ) -> RemoteResult<()> {
        let result = self.pointer_action_inner(action, serial);
        if result.is_err() {
            self.revoke_pointer();
        }
        result
    }
    fn pointer_action_inner(
        &mut self,
        action: &super::wayland_pointer_wire::PointerAction,
        serial: u64,
    ) -> RemoteResult<()> {
        self.sync_until(Instant::now() + PROTOCOL_TIMEOUT)?;
        let epoch = u32::try_from(action.lease_epoch)?;
        let sequence = u32::try_from(action.sequence)?;
        let (actual_serial, at) = self
            .pointer_frame()
            .ok_or_else(|| invalid("missing native pointer frame"))?;
        if self.pointer_revoked()
            || (action.kind.is_scroll()
                && !(if action.kind.is_horizontal() {
                    self.state.pointer.horizontal_enabled
                } else {
                    self.state.pointer.scroll_enabled
                }))
            || self.state.pointer.epoch != Some(epoch)
            || actual_serial != serial
            || at.elapsed() >= super::wayland_pointer_wire::FRAME_MAX_AGE
            || self.state.pointer.accepted.checked_add(1) != Some(sequence)
            || sequence > 60000
        {
            return Err(invalid("stale native pointer action").into());
        }
        self.state.pointer.expected = Some(sequence);
        self.pointer_lease
            .as_ref()
            .ok_or_else(|| invalid("missing pointer lease"))?
            .action(
                epoch,
                sequence,
                self.retained_frame.as_ref().unwrap(),
                action.kind as u32,
                u32::from(action.x),
                u32::from(action.y),
            );
        let deadline =
            (Instant::now() + PROTOCOL_TIMEOUT).min(self.state.pointer.deadline.unwrap());
        loop {
            self.state.check()?;
            if self.pointer_revoked() {
                return Err(invalid("native pointer revoked").into());
            }
            if self.state.pointer.accepted == sequence {
                break;
            }
            if Instant::now() >= deadline {
                return Err(invalid("native pointer action timed out").into());
            }
            self.pump(deadline)?;
        }
        self.state.pointer.expected = None;
        self.discard_pointer_frame();
        Ok(())
    }

    /// Evidence only for the actual retained first native keyboard capture.
    pub(super) fn keyboard_proof_serial(&self) -> Option<u64> {
        (self.state.keyboard.enabled
            && !self.state.keyboard.revoked
            && !self.state.keyboard.requested
            && self.state.frame_serial == 1
            && self.retained_frame.is_some())
        .then_some(self.state.frame_serial)
    }
    pub(super) fn enable_keyboard(&mut self) -> RemoteResult<()> {
        if self.state.interactive_popup.enabled {
            return Err(invalid("interactive family cannot upgrade input").into());
        }
        if self.state.text.enabled
            || self.state.resize_follow
            || self.state.popups
            || self.state.frame_serial != 0
            || self.state.control.enabled
            || self.state.keyboard.enabled
            || self.state.keyboard.revoked
            || self.state.keyboard.manager.is_none()
        {
            return Err(invalid("native keyboard manager unavailable or enable too late").into());
        }
        self.state.keyboard.enabled = true;
        Ok(())
    }

    pub(super) fn keyboard_revoked(&self) -> bool {
        self.state.keyboard.enabled
            && (self.state.keyboard.revoked
                || self
                    .arm_deadline
                    .is_some_and(|deadline| Instant::now() >= deadline)
                || self
                    .state
                    .keyboard
                    .deadline
                    .is_some_and(|deadline| Instant::now() >= deadline))
    }

    pub(super) fn revoke_keyboard(&mut self) {
        self.state.keyboard.revoked = true;
        if let Some(lease) = self.keyboard_lease.take() {
            lease.destroy();
        }
        if let Some(frame) = self.retained_frame.take() {
            frame.destroy();
        }
        self.arm_deadline = None;
        let _ = self.conn.backend().flush();
    }

    fn expire_keyboard(&mut self) {
        if self.keyboard_revoked() {
            self.revoke_keyboard();
        }
    }

    fn await_keyboard(
        &mut self,
        deadline: Instant,
        done: impl Fn(&NativeKeyboard) -> bool,
    ) -> RemoteResult<()> {
        loop {
            self.expire_text();
            self.expire_control();
            self.expire_keyboard();
            self.state.check()?;
            if self.state.keyboard.revoked {
                return Err(invalid("native keyboard lease revoked").into());
            }
            if done(&self.state.keyboard) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(invalid("native keyboard response timed out").into());
            }
            self.pump(deadline)?;
        }
    }

    pub(super) fn arm_keyboard(&mut self, digest: [u8; 32]) -> RemoteResult<u64> {
        let result = self.arm_keyboard_inner(digest);
        if result.is_err() {
            self.revoke_keyboard();
        }
        result
    }
    fn arm_keyboard_inner(&mut self, digest: [u8; 32]) -> RemoteResult<u64> {
        self.expire_text();
        self.expire_control();
        self.expire_keyboard();
        if !self.state.keyboard.enabled
            || self.state.keyboard.revoked
            || self.state.keyboard.requested
            || self.retained_frame.is_none()
            || digest == [0; 32]
        {
            return Err(invalid("native keyboard arm not permitted").into());
        }
        self.state.keyboard.requested = true;
        let deadline = (Instant::now() + PROTOCOL_TIMEOUT).min(self.arm_deadline.unwrap());
        let lease = self.state.keyboard.manager.as_ref().unwrap().create_lease(
            self.source.as_ref().unwrap(),
            &self.queue.handle(),
            (),
        );
        self.keyboard_lease = Some(lease);
        self.await_keyboard(deadline, |state| {
            state.digest.is_some() && state.epoch.is_some()
        })?;
        if self.state.keyboard.digest != Some(digest) {
            return Err(invalid("native and viewer full keymaps differ").into());
        }
        let epoch = self.state.keyboard.epoch.unwrap();
        self.state.keyboard.arm_sent = true;
        self.keyboard_lease
            .as_ref()
            .unwrap()
            .arm(epoch, self.retained_frame.as_ref().unwrap());
        self.await_keyboard(deadline, |state| state.armed)?;
        self.retained_frame.take().unwrap().destroy();
        self.arm_deadline = None;
        Ok(u64::from(epoch))
    }

    pub(super) fn keyboard_key(
        &mut self,
        key: &super::wayland_keyboard_wire::Key,
    ) -> RemoteResult<()> {
        let result = self.keyboard_key_inner(key);
        if result.is_err() {
            self.revoke_keyboard();
        }
        result
    }
    fn keyboard_key_inner(&mut self, key: &super::wayland_keyboard_wire::Key) -> RemoteResult<()> {
        self.expire_text();
        self.expire_control();
        self.expire_keyboard();
        let epoch = u32::try_from(key.lease_epoch)?;
        let sequence = u32::try_from(key.sequence)?;
        if self.state.keyboard.revoked
            || !self.state.keyboard.armed
            || self.state.keyboard.epoch != Some(epoch)
            || sequence == 0
            || sequence > 60_000
            || self.state.keyboard.accepted.checked_add(1) != Some(sequence)
            || !crate::remote_keyboard_protocol::keyboard_key_allowed(u32::from(key.keycode))
        {
            return Err(invalid("native keyboard edge not permitted").into());
        }
        self.state.keyboard.expected = Some(sequence);
        self.keyboard_lease
            .as_ref()
            .ok_or_else(|| invalid("missing native keyboard lease"))?
            .key(
                epoch,
                sequence,
                u32::from(key.keycode),
                u32::from(key.pressed),
            );
        let deadline =
            (Instant::now() + PROTOCOL_TIMEOUT).min(self.state.keyboard.deadline.unwrap());
        self.await_keyboard(deadline, |state| state.accepted == sequence)?;
        self.state.keyboard.expected = None;
        Ok(())
    }

    fn create_buffer(&mut self) -> RemoteResult<()> {
        let size = self
            .state
            .size
            .ok_or_else(|| invalid("missing Wayland buffer dimensions"))?;
        let (stride, bytes) = buffer_layout(u32::from(size.0), u32::from(size.1))?;
        let file = File::from(memfd_create("jwm-remote-wayland", MFdFlags::MFD_CLOEXEC)?);
        file.set_len(bytes as u64)?;
        let qh = self.queue.handle();
        let pool = self.state.shm.as_ref().unwrap().create_pool(
            file.as_fd(),
            i32::try_from(bytes)?,
            &qh,
            (),
        );
        let buffer = pool.create_buffer(
            0,
            i32::from(size.0),
            i32::from(size.1),
            stride,
            wl_shm::Format::Xrgb8888,
            &qh,
            (),
        );
        pool.destroy();
        self.file = Some(file);
        self.buffer = Some(buffer);
        Ok(())
    }

    fn read_selection(&mut self, deadline: Instant) -> RemoteResult<usize> {
        let stdin = io::stdin();
        let mut bytes = Vec::with_capacity(MAX_SELECTION_BYTES);
        loop {
            self.state.check()?;
            if Instant::now() >= deadline {
                return Err(invalid("Wayland window selection timed out").into());
            }
            // Continue lifecycle dispatch while waiting for local authorization.
            self.pump(deadline.min(Instant::now() + Duration::from_millis(25)))?;
            let mut fds = [PollFd::new(
                stdin.as_fd(),
                PollFlags::POLLIN | PollFlags::POLLHUP | PollFlags::POLLERR,
            )];
            if poll(&mut fds, 0_u16)? == 0 {
                continue;
            }
            let mut byte = [0_u8; 1];
            if nix::unistd::read(stdin.as_fd(), &mut byte)? != 1 {
                return Err(invalid("Wayland window selection cancelled").into());
            }
            bytes.push(byte[0]);
            if bytes.len() > MAX_SELECTION_BYTES {
                return Err(invalid("Wayland window selection exceeds 32 bytes").into());
            }
            if byte[0] == b'\n' {
                return parse_selection(&bytes);
            }
            if bytes.len() == MAX_SELECTION_BYTES {
                return Err(invalid("Wayland window selection exceeds 32 bytes").into());
            }
        }
    }

    fn sync_until(&mut self, deadline: Instant) -> RemoteResult<()> {
        let expected = self
            .state
            .sync_count
            .checked_add(1)
            .ok_or_else(|| invalid("Wayland sync counter exhausted"))?;
        self.conn.display().sync(&self.queue.handle(), ());
        self.drive_until(deadline, |state| state.sync_count >= expected)
    }

    fn drive_until(
        &mut self,
        deadline: Instant,
        done: impl Fn(&State) -> bool,
    ) -> RemoteResult<()> {
        loop {
            self.state.check()?;
            if done(&self.state) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(invalid("Wayland protocol deadline exceeded").into());
            }
            self.pump(deadline)?;
        }
    }

    /// Never use blocking_dispatch or roundtrip: both allow an unbounded wait.
    fn pump(&mut self, deadline: Instant) -> RemoteResult<bool> {
        // Protocol waits must never be satisfied by host input wakeups.
        self.pump_activity(deadline, None)
    }

    /// The optional fd is exclusive to host activity waits. It is borrowed,
    /// never drained here, and cannot satisfy a capture/acceptance/sync wait.
    fn pump_activity(
        &mut self,
        deadline: Instant,
        wake: Option<BorrowedFd<'_>>,
    ) -> RemoteResult<bool> {
        let dispatched = self.queue.dispatch_pending(&mut self.state)?;
        self.state.check()?;
        if dispatched > 0 {
            return Ok(true);
        }
        let backend = self.conn.backend();
        if backend.dispatch_inner_queue()? > 0 {
            self.queue.dispatch_pending(&mut self.state)?;
            self.state.check()?;
            return Ok(true);
        }
        let Some(guard) = backend.prepare_read() else {
            self.queue.dispatch_pending(&mut self.state)?;
            self.state.check()?;
            return Ok(true);
        };
        let needs_write = match backend.flush() {
            Ok(()) => false,
            Err(WaylandError::Io(error)) if error.kind() == io::ErrorKind::WouldBlock => true,
            Err(error) => return Err(error.into()),
        };
        let mut flags = PollFlags::POLLIN | PollFlags::POLLERR | PollFlags::POLLHUP;
        if needs_write {
            flags |= PollFlags::POLLOUT;
        }
        // Keep ordinary protocol pumps allocation-free and single-fd. The
        // second stack entry is outside the polled slice when no wake exists.
        let mut storage = [
            PollFd::new(backend.poll_fd(), flags),
            PollFd::new(
                wake.unwrap_or_else(|| backend.poll_fd()),
                PollFlags::POLLIN | PollFlags::POLLERR | PollFlags::POLLHUP,
            ),
        ];
        let fds = &mut storage[..if wake.is_some() { 2 } else { 1 }];
        loop {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return Ok(false);
            };
            let millis = remaining
                .as_millis()
                .saturating_add(1)
                .min(i32::MAX as u128) as i32;
            let timeout = PollTimeout::try_from(millis).unwrap_or(PollTimeout::MAX);
            match poll(fds, timeout) {
                Ok(0) => return Ok(false),
                Ok(_) => break,
                Err(nix::errno::Errno::EINTR) => continue,
                Err(error) => return Err(error.into()),
            }
        }
        let ready = activity_readiness(
            fds[0].revents().unwrap_or(PollFlags::empty()),
            fds.get(1)
                .and_then(PollFd::revents)
                .unwrap_or(PollFlags::empty()),
        );
        if ready.native_failed {
            return Err(invalid("Wayland compositor disconnected").into());
        }
        if ready.read_native {
            match guard.read() {
                Ok(_) => {}
                Err(WaylandError::Io(error)) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error.into()),
            }
        } else {
            drop(guard);
        }
        self.queue.dispatch_pending(&mut self.state)?;
        self.state.check()?;
        if ready.wake_failed {
            return Err(invalid("host activity wake descriptor failed").into());
        }
        Ok(true)
    }

    pub(super) fn frame(&mut self) -> RemoteResult<CaptureOutcome> {
        self.frame_with_pointer_visual(false)
    }
    pub(super) fn frame_with_control_visual(&mut self) -> RemoteResult<CaptureOutcome> {
        if !self.state.control.enabled
            || self.control_revoked()
            || self.retained_frame.is_some()
            || self.state.pointer.enabled
            || self.state.text.enabled
            || self.state.keyboard.enabled
        {
            return Err(invalid("visual control repair lacks quiesced capture authority").into());
        }
        self.frame_with_visual_inner(true)
    }
    pub(super) fn frame_with_pointer_visual(
        &mut self,
        visual_only: bool,
    ) -> RemoteResult<CaptureOutcome> {
        if visual_only
            && (!self.state.pointer.enabled
                || self.pointer_revoked()
                || self.retained_frame.is_some()
                || self.state.control.enabled
                || self.state.text.enabled
                || self.state.keyboard.enabled)
        {
            return Err(invalid("visual pointer repair lacks quiesced capture authority").into());
        }
        self.frame_with_visual_inner(visual_only)
    }
    fn frame_with_visual_inner(&mut self, visual_only: bool) -> RemoteResult<CaptureOutcome> {
        let result = self.read_frame_with_pointer_visual(visual_only);
        if result.is_err() {
            if self.state.interactive_popup.enabled {
                self.fail_interactive_popup();
            }
            self.revoke_keyboard();
            self.state
                .fail("Wayland capture failed; local authorization is required again");
            if let Some(frame) = self.inflight.take() {
                frame.destroy();
            }
        }
        result
    }

    fn rebuild_resize_session(&mut self, deadline: Instant) -> RemoteResult<()> {
        if !self.state.resize_follow
            || self.state.follow_suspended
            || self.state.follow_layout.is_none()
            || self.state.follow_epoch <= self.applied_follow_epoch
        {
            return Err(invalid("resize-follow rebuild requires stopped session").into());
        }
        self.resize_sessions = self
            .resize_sessions
            .checked_add(1)
            .filter(|count| *count <= 1024)
            .ok_or_else(|| invalid("Wayland resize-follow session budget exhausted"))?;
        if let Some(frame) = self.inflight.take() {
            frame.destroy();
        }
        if let Some(session) = self.session.take() {
            session.destroy();
        }
        if let Some(buffer) = self.buffer.take() {
            buffer.destroy();
        }
        // Drain old callbacks and acknowledge old SHM retirement before any new
        // session/buffer. Keep the same derived source and same connection.
        self.sync_until(deadline)?;
        self.file = None;
        self.state.size = None;
        self.state.batch_size = None;
        self.state.batch_xrgb = false;
        self.state.constraints_done = false;
        self.state.session_stopped = false;
        self.state.frame_failed = false;
        self.state.frame_ready = false;
        let transitions = u64::from(self.state.follow_epoch - self.applied_follow_epoch);
        self.epoch = self
            .epoch
            .checked_add(transitions)
            .ok_or_else(|| invalid("Wayland geometry epoch exhausted"))?;
        self.applied_follow_epoch = self.state.follow_epoch;
        self.session = Some(self.state.capture_manager.as_ref().unwrap().create_session(
            self.source.as_ref().unwrap(),
            manager::Options::empty(),
            &self.queue.handle(),
            (),
        ));
        self.drive_until(deadline, |state| {
            state.constraints_done || state.session_stopped
        })?;
        if self.state.session_stopped {
            return Err(invalid(
                "Wayland resize-follow source revoked or unstable during bounded rebuild",
            )
            .into());
        }
        if self.state.size != self.state.follow_layout {
            return Err(
                invalid("Wayland resize-follow constraints disagree with native layout").into(),
            );
        }
        self.create_buffer()?;
        self.sync_until(deadline)?;
        if self.state.session_stopped {
            return Err(invalid("Wayland resize-follow stopped during buffer creation").into());
        }
        Ok(())
    }
    fn read_frame_with_pointer_visual(
        &mut self,
        visual_only: bool,
    ) -> RemoteResult<CaptureOutcome> {
        self.state.check()?;
        self.expire_text();
        self.expire_control();
        self.expire_keyboard();
        if self.state.pointer.enabled && self.pointer_revoked() {
            self.revoke_pointer();
        }
        if self.retained_frame.is_some() {
            self.pump(Instant::now() + Duration::from_millis(1))?;
            self.expire_text();
            self.expire_control();
            self.expire_keyboard();
            return Ok(CaptureOutcome::RetainedProofBlocked);
        }
        if self.inflight.is_some() {
            return Err(invalid("duplicate Wayland capture frame").into());
        }
        let deadline = self.popup_protocol_deadline()?;
        self.sync_until(deadline)?;
        if self.state.resize_follow {
            if self.state.follow_suspended {
                return Ok(CaptureOutcome::NoChange);
            }
            if self.state.follow_epoch > self.applied_follow_epoch {
                self.rebuild_resize_session(deadline)?;
            } else if self.state.session_stopped {
                return Err(invalid(
                    "Wayland capture session stopped without a newer native layout",
                )
                .into());
            }
        }
        self.drive_until(deadline, |state| {
            state.constraints_done || state.session_stopped
        })?;
        if self.state.session_stopped {
            return Ok(CaptureOutcome::NoChange);
        }
        self.state.frame_failed = false;
        self.state.frame_serial = self
            .state
            .frame_serial
            .checked_add(1)
            .ok_or_else(|| invalid("Wayland frame sequence exhausted"))?;
        self.state.frame_ready = false;
        self.state.normal_transform = false;
        let frame = self
            .session
            .as_ref()
            .unwrap()
            .create_frame(&self.queue.handle(), self.state.frame_serial);
        let (width, height) = self.state.size.unwrap();
        frame.attach_buffer(self.buffer.as_ref().unwrap());
        frame.damage_buffer(0, 0, i32::from(width), i32::from(height));
        frame.capture();
        self.inflight = Some(frame);
        self.drive_until(deadline, |state| {
            state.frame_ready || state.frame_failed || state.session_stopped
        })?;
        if self.state.resize_follow {
            self.sync_until(deadline)?;
            if self.state.session_stopped {
                self.inflight.take().unwrap().destroy();
                return Ok(CaptureOutcome::NoChange);
            }
            if self.state.frame_failed {
                return Err(
                    invalid("Wayland capture failed without a stopped resize session").into(),
                );
            }
        }
        let ready_frame = self.inflight.take().unwrap();
        if self.state.interactive_popup.enabled {
            self.retained_frame = Some(ready_frame);
        } else if self.state.text.enabled && !self.state.text.ended && !self.state.text.revoked {
            self.retained_frame = Some(ready_frame);
            self.pointer_ready_at = Some(Instant::now());
        } else if self.state.control.enabled && !self.control_revoked() && !visual_only {
            self.retained_frame = Some(ready_frame);
            self.pointer_ready_at = Some(Instant::now());
            if self.state.frame_serial == 1 {
                self.arm_deadline = Some(Instant::now() + PROTOCOL_TIMEOUT);
            }
        } else if self.state.pointer.enabled && !self.pointer_revoked() && !visual_only {
            self.retained_frame = Some(ready_frame);
            self.pointer_ready_at = Some(Instant::now());
        } else if self.state.keyboard.enabled
            && !self.state.keyboard.revoked
            && self.state.frame_serial == 1
        {
            self.retained_frame = Some(ready_frame);
            self.arm_deadline = Some(Instant::now() + PROTOCOL_TIMEOUT);
        } else {
            ready_frame.destroy();
        }
        // Ready permits buffer reuse (wl_buffer.release is explicitly unused).
        // A final ordered barrier catches already-issued closed/stopped events.
        self.sync_until(deadline)?;
        self.state.check()?;
        if self.state.resize_follow && self.state.session_stopped {
            return Ok(CaptureOutcome::NoChange);
        }
        if !self.state.constraints_done {
            return Err(invalid("Wayland buffer constraints changed during capture").into());
        }
        let (_, bytes) = buffer_layout(u32::from(width), u32::from(height))?;
        let mut pixels = Vec::new();
        pixels.try_reserve_exact(bytes)?;
        pixels.resize(bytes, 0);
        self.file.as_ref().unwrap().read_exact_at(&mut pixels, 0)?;
        let rgb = decode_xrgb(&pixels, width, height)?;
        let (out_width, out_height) =
            frame_dimensions(width, height, self.max_width, self.max_height, self.limits);
        self.limits.validate_dimensions(out_width, out_height)?;
        let image = if (out_width, out_height) == (width, height) {
            rgb
        } else {
            let resized = image::imageops::resize(
                &rgb,
                out_width.into(),
                out_height.into(),
                FilterType::Triangle,
            );
            self.recycler.recycle_image(rgb);
            resized
        };
        self.sync_until(deadline)?;
        self.state.check()?;
        if self.state.resize_follow && self.state.session_stopped {
            return Ok(CaptureOutcome::NoChange);
        }
        if self.state.interactive_popup.enabled {
            if (out_width, out_height) != (width, height) {
                return Err(invalid("interactive family scaling is forbidden").into());
            }
            self.export_interactive_popup_frame(deadline)?;
        }
        #[cfg(all(test, feature = "backend-wayland-udev"))]
        CAPTURE_FRAME_OBSERVER.with(|slot| {
            if let Some(observer) = slot.borrow().as_ref() {
                let mut frames = observer.lock().unwrap();
                assert!(frames.len() < 4096, "test capture observer bound");
                frames.push((u32::from(width), u32::from(height), self.epoch));
            }
        });
        Ok(CaptureOutcome::Frame(CapturedFrame {
            geometry_epoch: self.epoch,
            image,
            source_width: width,
            source_height: height,
        }))
    }

    fn advance_epoch(&mut self) -> bool {
        if self.state.frame_serial > 0 && self.state.keyboard.enabled {
            self.revoke_keyboard();
        }
        if let Some(epoch) = self.epoch.checked_add(1) {
            self.epoch = epoch;
            true
        } else {
            self.state.fail("Wayland geometry epoch exhausted");
            false
        }
    }
    pub(super) fn set_frame_limits(&mut self, limits: FrameLimits) -> bool {
        let limits = self.limits.intersect(limits);
        if limits.validate().is_err() {
            self.state.fail("invalid Wayland frame limits");
            return false;
        }
        if limits == self.limits {
            return false;
        }
        self.limits = limits;
        self.advance_epoch()
    }
    pub(super) fn set_encoded_bounds(&mut self, width: u16, height: u16) -> bool {
        let width = match (self.host_width_limit, width) {
            (0, n) | (n, 0) => n,
            (a, b) => a.min(b),
        };
        if (width, height) == (self.max_width, self.max_height) {
            return false;
        }
        self.max_width = width;
        self.max_height = height;
        self.advance_epoch()
    }
    pub(super) fn geometry_epoch(&self) -> u64 {
        // Publish invalidation as soon as suspended is observed, before the
        // application commits its replacement layout or new pixels are read.
        self.epoch
            .checked_add(u64::from(
                self.state
                    .follow_epoch
                    .saturating_sub(self.applied_follow_epoch),
            ))
            .unwrap_or(u64::MAX)
    }
    pub(crate) fn image_recycler(&self) -> CaptureImageRecycler {
        self.recycler.clone()
    }
    pub(super) fn wait_for_activity_with_wake(
        &mut self,
        timeout: Duration,
        wake: Option<BorrowedFd<'_>>,
    ) -> RemoteResult<bool> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| invalid("invalid Wayland wait timeout"))?;
        self.expire_text();
        self.expire_control();
        self.expire_keyboard();
        let deadline = self
            .arm_deadline
            .into_iter()
            .chain(self.state.keyboard.deadline)
            .fold(deadline, |a, b| a.min(b));
        let result = self.pump_activity(deadline, wake);
        self.expire_text();
        self.expire_control();
        self.expire_keyboard();
        if result.is_err() {
            self.state.fail("Wayland capture connection failed");
        }
        result
    }
}

impl Drop for WaylandCapture {
    fn drop(&mut self) {
        self.finish_interactive_popup();
        self.finish_text();
        self.revoke_control();
        self.revoke_pointer();
        self.revoke_keyboard();
        if let Some(frame) = self.inflight.take() {
            frame.destroy();
        }
        if let Some(session) = self.session.take() {
            session.destroy();
        }
        if let Some(source) = self.source.take() {
            source.destroy();
        }
        if let Some(buffer) = self.buffer.take() {
            buffer.destroy();
        }
        let _ = self.conn.backend().flush();
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &wl_registry::WlRegistry,
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
            } if version >= 1 => {
                let bound = match interface.as_str() {
                    "jwm_remote_text_manager_v1" if state.text.manager.is_none() => {
                        state.text.global = Some(name);
                        state.text.manager = Some(proxy.bind(name, 1, qh, ()));
                        false
                    }
                    "jwm_remote_popup_manager_v1"
                        if state.interactive_popup.enabled
                            && version >= 2
                            && state.interactive_popup.manager.is_none() =>
                    {
                        state.interactive_popup.manager = Some(proxy.bind(name, 2, qh, ()));
                        true
                    }
                    "jwm_readonly_popup_capture_manager_v1"
                        if state.popups
                            && !state.resize_follow
                            && state.popup_manager.is_none() =>
                    {
                        state.popup_manager = Some(proxy.bind(name, 1, qh, ()));
                        true
                    }
                    "jwm_readonly_resize_capture_manager_v1"
                        if state.resize_follow && state.resize_manager.is_none() =>
                    {
                        state.resize_manager = Some(proxy.bind(name, version.min(2), qh, ()));
                        true
                    }
                    "jwm_remote_control_manager_v1" if state.control.manager.is_none() => {
                        state.control.manager = Some(proxy.bind(name, version.min(3), qh, ()));
                        state.control.global = Some(name);
                        false
                    }
                    "jwm_remote_pointer_manager_v1" if state.pointer.manager.is_none() => {
                        state.pointer.manager = Some(proxy.bind(name, version.min(3), qh, ()));
                        state.pointer.global = Some(name);
                        false
                    }
                    "jwm_remote_keyboard_manager_v1" if state.keyboard.manager.is_none() => {
                        state.keyboard.manager = Some(proxy.bind(name, 1, qh, ()));
                        state.keyboard.global = Some(name);
                        false
                    }
                    "wl_shm" if state.shm.is_none() => {
                        state.shm = Some(proxy.bind(name, 1, qh, ()));
                        true
                    }
                    "ext_foreign_toplevel_list_v1" if state.list.is_none() => {
                        state.list = Some(proxy.bind(name, 1, qh, ()));
                        true
                    }
                    "ext_foreign_toplevel_image_capture_source_manager_v1"
                        if state.source_manager.is_none() =>
                    {
                        state.source_manager = Some(proxy.bind(name, 1, qh, ()));
                        true
                    }
                    "ext_image_copy_capture_manager_v1" if state.capture_manager.is_none() => {
                        state.capture_manager = Some(proxy.bind(name, 1, qh, ()));
                        true
                    }
                    _ => false,
                };
                if bound {
                    state.globals.push(name);
                }
            }
            wl_registry::Event::GlobalRemove { name } if state.text.global == Some(name) => {
                state.text.revoked = true;
                state.text.manager = None;
            }
            wl_registry::Event::GlobalRemove { name } if state.control.global == Some(name) => {
                state.control.revoked = true;
                state.control.manager = None;
            }
            wl_registry::Event::GlobalRemove { name } if state.pointer.global == Some(name) => {
                state.pointer.revoked = true;
                state.pointer.manager = None;
            }
            wl_registry::Event::GlobalRemove { name } if state.keyboard.global == Some(name) => {
                state.keyboard.revoked = true;
                state.keyboard.manager = None;
            }
            wl_registry::Event::GlobalRemove { name } if state.globals.contains(&name) => {
                state.fail("required Wayland global was removed")
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        _: wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let Some(next) = state.sync_count.checked_add(1) {
            state.sync_count = next;
        } else {
            state.fail("Wayland sync counter exhausted");
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
            state.shm_xrgb = true;
        }
    }
}
impl Dispatch<ExtForeignToplevelListV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtForeignToplevelListV1,
        event: list::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            list::Event::Toplevel { toplevel } => {
                if state.toplevels.len() >= MAX_TOPLEVELS {
                    state.fail("Wayland toplevel list exceeds 256 entries");
                    return;
                }
                state.toplevels.push(Toplevel {
                    handle: toplevel,
                    title: String::new(),
                    app_id: String::new(),
                    complete: false,
                    closed: false,
                });
            }
            list::Event::Finished => state.fail("Wayland toplevel list stopped"),
            _ => {}
        }
    }
    wayland_client::event_created_child!(State, ExtForeignToplevelListV1, [0 => (ExtForeignToplevelHandleV1, ())]);
}
impl Dispatch<ExtForeignToplevelHandleV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &ExtForeignToplevelHandleV1,
        event: handle::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(item) = state
            .toplevels
            .iter_mut()
            .find(|item| &item.handle == proxy)
        else {
            state.fail("unknown Wayland toplevel resource");
            return;
        };
        match event {
            handle::Event::Title { title } => item.title = safe_label(&title),
            handle::Event::AppId { app_id } => item.app_id = safe_label(&app_id),
            handle::Event::Done => item.complete = true,
            handle::Event::Closed => {
                item.closed = true;
                if state.selected.as_ref() == Some(proxy) {
                    state.fail("selected Wayland window closed");
                }
            }
            // Identifier is deliberately never stored or used for selection.
            _ => {}
        }
    }
}
impl Dispatch<ExtImageCopyCaptureSessionV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtImageCopyCaptureSessionV1,
        event: session::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            session::Event::BufferSize { width, height } => state.advertise_size(width, height),
            session::Event::ShmFormat { format } => {
                state.begin_constraints();
                state.batch_xrgb |= format == WEnum::Value(wl_shm::Format::Xrgb8888);
            }
            session::Event::DmabufDevice { .. } | session::Event::DmabufFormat { .. } => {
                state.begin_constraints()
            }
            session::Event::Done => {
                if let Some(size) = state.batch_size.filter(|_| state.batch_xrgb) {
                    state.size = Some(size);
                    state.constraints_done = true;
                } else {
                    state.fail("Wayland session requires bounded XRGB8888 SHM constraints");
                }
            }
            session::Event::Stopped if state.resize_follow => state.session_stopped = true,
            session::Event::Stopped => state.fail("Wayland image capture session stopped"),
            _ => state.fail("unknown Wayland capture session event"),
        }
    }
}
impl Dispatch<ExtImageCopyCaptureFrameV1, u64> for State {
    fn event(
        state: &mut Self,
        _: &ExtImageCopyCaptureFrameV1,
        event: frame::Event,
        serial: &u64,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if *serial != state.frame_serial {
            state.fail("stale Wayland capture frame");
            return;
        }
        match event {
            frame::Event::Transform {
                transform: WEnum::Value(wl_output::Transform::Normal),
            } => state.normal_transform = true,
            frame::Event::Transform { .. } => state.fail("unsupported Wayland buffer transform"),
            frame::Event::Ready if state.normal_transform && !state.frame_ready => {
                state.frame_ready = true
            }
            frame::Event::Ready => {
                state.fail("Wayland frame missing normal transform or duplicated ready")
            }
            frame::Event::Failed { .. }
                if state.resize_follow && !state.frame_ready && !state.frame_failed =>
            {
                state.frame_failed = true
            }
            frame::Event::Failed { .. } => state.fail("Wayland compositor rejected capture frame"),
            frame::Event::Damage { .. } | frame::Event::PresentationTime { .. } => {}
            _ => state.fail("unknown Wayland frame event"),
        }
    }
}
wayland_client::delegate_noop!(State: ignore wl_buffer::WlBuffer);
wayland_client::delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
wayland_client::delegate_noop!(State: ignore ExtForeignToplevelImageCaptureSourceManagerV1);
wayland_client::delegate_noop!(State: ignore ExtImageCaptureSourceV1);
wayland_client::delegate_noop!(State: ignore ExtImageCopyCaptureManagerV1);
impl Dispatch<JwmReadonlyResizeCaptureManagerV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &JwmReadonlyResizeCaptureManagerV1,
        event: resize_manager::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        state.follow_event(event);
    }
}

fn socket_path(display: Option<&str>) -> RemoteResult<PathBuf> {
    let name = display
        .map(str::to_owned)
        .or_else(|| std::env::var("WAYLAND_DISPLAY").ok())
        .unwrap_or_else(|| "wayland-0".to_owned());
    if name.is_empty() || name.as_bytes().contains(&0) {
        return Err(invalid("invalid Wayland socket name").into());
    }
    let path = Path::new(&name);
    if path.is_absolute() {
        return Ok(path.to_owned());
    }
    if path.components().count() != 1 {
        return Err(invalid("relative Wayland socket must be a single name").into());
    }
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .ok_or_else(|| invalid("XDG_RUNTIME_DIR is required for Wayland"))?;
    let runtime = PathBuf::from(runtime);
    if !runtime.is_absolute() {
        return Err(invalid("XDG_RUNTIME_DIR must be absolute").into());
    }
    Ok(runtime.join(path))
}
fn parse_selection(bytes: &[u8]) -> RemoteResult<usize> {
    if bytes.is_empty() || bytes.len() > MAX_SELECTION_BYTES || bytes.last() != Some(&b'\n') {
        return Err(invalid("invalid Wayland window selection line").into());
    }
    let digits = bytes
        .strip_suffix(b"\n")
        .unwrap()
        .strip_suffix(b"\r")
        .unwrap_or(&bytes[..bytes.len() - 1]);
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return Err(invalid("Wayland window selection must be a decimal index").into());
    }
    let mut number = 0_usize;
    for digit in digits {
        number = number
            .checked_mul(10)
            .and_then(|n| n.checked_add(usize::from(digit - b'0')))
            .ok_or_else(|| invalid("Wayland window index overflow"))?;
    }
    if number == 0 || number > MAX_TOPLEVELS {
        return Err(invalid("Wayland window index out of range").into());
    }
    Ok(number - 1)
}
fn safe_label(value: &str) -> String {
    let mut output = String::new();
    for character in value.chars() {
        let escaped = character.escape_default().to_string();
        if output.len() + escaped.len() > MAX_LABEL_BYTES - 3 {
            output.push_str("...");
            break;
        }
        output.push_str(&escaped);
    }
    output
}
fn buffer_layout(width: u32, height: u32) -> RemoteResult<(i32, usize)> {
    let width = u16::try_from(width)?;
    let height = u16::try_from(height)?;
    super::frame::validate_dimensions(width, height)?;
    let stride = u32::from(width)
        .checked_mul(4)
        .ok_or_else(|| invalid("Wayland stride overflow"))?;
    let bytes = u64::from(stride)
        .checked_mul(u64::from(height))
        .ok_or_else(|| invalid("Wayland SHM size overflow"))?;
    let bytes = usize::try_from(bytes)?;
    i32::try_from(bytes)?;
    Ok((i32::try_from(stride)?, bytes))
}
fn decode_xrgb(bytes: &[u8], width: u16, height: u16) -> RemoteResult<RgbImage> {
    let (_, expected) = buffer_layout(u32::from(width), u32::from(height))?;
    if bytes.len() != expected {
        return Err(invalid("Wayland SHM buffer length changed").into());
    }
    let mut rgb = Vec::new();
    rgb.try_reserve_exact(expected / 4 * 3)?;
    for pixel in bytes.chunks_exact(4) {
        let value = u32::from_ne_bytes(pixel.try_into().unwrap());
        rgb.extend_from_slice(&[(value >> 16) as u8, (value >> 8) as u8, value as u8]);
    }
    RgbImage::from_raw(u32::from(width), u32::from(height), rgb)
        .ok_or_else(|| invalid("invalid decoded Wayland image").into())
}
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn resize_follow_layout_requires_monotonic_suspension_and_stopped_never_revives() {
        let mut state = State {
            resize_follow: true,
            ..Default::default()
        };
        for epoch in 1..=3 {
            state.follow_event(resize_manager::Event::Suspended { epoch });
            assert!(state.follow_suspended && state.session_stopped);
            assert!(state.follow_layout.is_none());
            state.follow_event(resize_manager::Event::Layout {
                epoch,
                width: 320,
                height: 240,
            });
            assert!(!state.follow_suspended);
            assert_eq!(state.follow_epoch, epoch);
            assert_eq!(state.follow_layout, Some((320, 240)));
            assert!(state.check().is_ok());
        }
        state.follow_event(resize_manager::Event::Stopped);
        state.follow_event(resize_manager::Event::Suspended { epoch: 4 });
        state.follow_event(resize_manager::Event::Layout {
            epoch: 4,
            width: 640,
            height: 480,
        });
        assert!(state.check().is_err());
        assert_eq!(state.follow_epoch, 3);
    }
    #[test]
    fn resize_follow_rejects_unsolicited_stale_or_unbounded_layout() {
        for case in 0..4 {
            let mut state = State {
                resize_follow: true,
                ..Default::default()
            };
            if case != 0 {
                state.follow_event(resize_manager::Event::Suspended { epoch: 1 });
            }
            if case == 3 {
                state.follow_event(resize_manager::Event::Suspended { epoch: 1 });
            }
            state.follow_event(resize_manager::Event::Layout {
                epoch: if case == 1 { 2 } else { 1 },
                width: if case == 2 { 0 } else { 320 },
                height: 240,
            });
            assert!(state.check().is_err(), "case {case}");
        }
    }
    #[test]
    fn selection_is_bounded_decimal_and_never_a_title_or_identifier() {
        assert_eq!(parse_selection(b"1\n").unwrap(), 0);
        assert_eq!(parse_selection(b"256\r\n").unwrap(), 255);
        for line in [
            b"0\n".as_slice(),
            b"257\n",
            b"1;quit\n",
            b" 1\n",
            b"\n",
            b"1",
            b"-1\n",
        ] {
            assert!(parse_selection(line).is_err());
        }
        assert!(parse_selection(&[b'1'; 33]).is_err());
    }
    #[test]
    fn labels_escape_terminal_control_and_are_bounded() {
        let label = safe_label("title\x1b[2J\n\r\t\0\x7f");
        assert!(!label.chars().any(char::is_control));
        assert!(label.contains("\\u{1b}"));
        assert!(safe_label(&"\x1b".repeat(1000)).len() <= MAX_LABEL_BYTES);
        assert!(safe_label(&"文".repeat(1000)).len() <= MAX_LABEL_BYTES);
    }
    #[test]
    fn shm_bounds_reject_zero_oversize_and_pixel_budget_overflow() {
        assert_eq!(buffer_layout(16, 8).unwrap(), (64, 512));
        for (width, height) in [(0, 1), (1, 0), (16_385, 1), (u32::MAX, 1), (16_384, 16_384)] {
            assert!(buffer_layout(width, height).is_err());
        }
    }
    #[test]
    fn xrgb_decoding_uses_native_endian_masks_not_cursor_or_alpha() {
        let bytes = [0x12ff0000_u32.to_ne_bytes(), 0xab00ff00_u32.to_ne_bytes()].concat();
        let image = decode_xrgb(&bytes, 2, 1).unwrap();
        assert_eq!(image.as_raw(), &[255, 0, 0, 0, 255, 0]);
        assert!(decode_xrgb(&bytes[..7], 2, 1).is_err());
    }
    #[test]
    fn initial_resize_restore_is_terminal_even_before_constraints_done() {
        let mut state = State::default();
        state.advertise_size(800, 600);
        assert!(state.check().is_ok());
        state.advertise_size(801, 600);
        assert!(state.check().is_err());
        state.advertise_size(800, 600);
        assert!(state.check().is_err());
        assert_eq!(state.size, Some((800, 600)));
    }
    #[test]
    fn stop_or_failure_latches_cannot_be_cleared_by_later_metadata() {
        let mut state = State::default();
        state.fail("selected Wayland window closed");
        state.fail("later event");
        state.advertise_size(640, 480);
        state.constraints_done = true;
        state.frame_ready = true;
        assert_eq!(state.failed, Some("selected Wayland window closed"));
        assert!(state.check().is_err());
    }
}

wayland_client::delegate_noop!(State: ignore JwmRemoteKeyboardManagerV1);
impl Dispatch<JwmRemoteKeyboardLeaseV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &JwmRemoteKeyboardLeaseV1,
        event: keyboard_lease::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let keyboard = &mut state.keyboard;
        if keyboard.revoked {
            return;
        }
        match event {
            keyboard_lease::Event::Keymap { format, fd, size } => {
                let read = || -> RemoteResult<[u8; 32]> {
                    if format != 1 || size == 0 || size > 1024 * 1024 {
                        return Err(invalid("invalid native keyboard map size or format").into());
                    }
                    let file = File::from(fd);
                    if !file.metadata()?.is_file() {
                        return Err(invalid("native keyboard map is not a regular file").into());
                    }
                    let mut bytes = vec![0; size as usize];
                    file.read_exact_at(&mut bytes, 0)?;
                    super::keyboard_keymap::canonical_digest(&bytes)
                };
                if keyboard.digest.is_some() {
                    keyboard.revoked = true;
                    return;
                }
                match read() {
                    Ok(digest) => keyboard.digest = Some(digest),
                    Err(_) => keyboard.revoked = true,
                }
            }
            keyboard_lease::Event::Granted { id, epoch, ttl_ms } => {
                if id == 0
                    || epoch == 0
                    || ttl_ms == 0
                    || ttl_ms > 30_000
                    || keyboard.epoch.is_some()
                {
                    keyboard.revoked = true;
                    return;
                }
                keyboard.epoch = Some(epoch);
                keyboard.deadline = Some(Instant::now() + Duration::from_millis(u64::from(ttl_ms)));
            }
            keyboard_lease::Event::Armed { epoch } => {
                if keyboard.epoch != Some(epoch)
                    || !keyboard.arm_sent
                    || keyboard.armed
                    || keyboard.digest.is_none()
                {
                    keyboard.revoked = true;
                } else {
                    keyboard.armed = true;
                }
            }
            keyboard_lease::Event::Accepted { epoch, sequence } => {
                if !keyboard.armed
                    || keyboard.epoch != Some(epoch)
                    || keyboard.expected != Some(sequence)
                    || keyboard.accepted.checked_add(1) != Some(sequence)
                {
                    keyboard.revoked = true;
                } else {
                    keyboard.accepted = sequence;
                }
            }
            keyboard_lease::Event::Revoked { .. } => keyboard.revoked = true,
        }
    }
}

wayland_client::delegate_noop!(State: ignore JwmRemotePointerManagerV1);
impl Dispatch<JwmRemotePointerLeaseV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &JwmRemotePointerLeaseV1,
        event: pointer_lease::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let pointer = &mut state.pointer;
        if pointer.revoked {
            return;
        }
        match event {
            pointer_lease::Event::Granted { epoch, ttl_ms } => {
                if !pointer.requested
                    || pointer.epoch.is_some()
                    || epoch == 0
                    || ttl_ms == 0
                    || ttl_ms > 30000
                {
                    pointer.revoked = true;
                    return;
                }
                pointer.epoch = Some(epoch);
                let deadline = Instant::now() + Duration::from_millis(u64::from(ttl_ms));
                pointer.deadline = Some(pointer.deadline.map_or(deadline, |old| old.min(deadline)));
            }
            pointer_lease::Event::Accepted { epoch, sequence } => {
                if pointer.epoch != Some(epoch)
                    || pointer.expected != Some(sequence)
                    || pointer.accepted.checked_add(1) != Some(sequence)
                {
                    pointer.revoked = true;
                    return;
                }
                pointer.accepted = sequence;
            }
            pointer_lease::Event::Revoked { reason } => {
                #[cfg(test)]
                eprintln!("native pointer revoked reason={reason}");
                let _ = reason;
                pointer.revoked = true;
            }
        }
    }
}

impl Dispatch<JwmReadonlyPopupCaptureManagerV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &JwmReadonlyPopupCaptureManagerV1,
        event: popup_manager::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            popup_manager::Event::Ready { width, height } => state.popup_ready(width, height),
            popup_manager::Event::Stopped => {
                state.fail("Wayland popup capture permanently stopped")
            }
        }
    }
}
impl State {
    fn popup_ready(&mut self, width: u32, height: u32) {
        if !self.popups
            || self.failed.is_some()
            || self.popup_layout.is_some()
            || width == 0
            || height == 0
            || width > 16384
            || height > 16384
            || u64::from(width) * u64::from(height) > 64 * 1024 * 1024
        {
            self.fail("invalid or repeated Wayland popup canvas");
        } else {
            self.popup_layout = Some((width as u16, height as u16));
        }
    }
}
#[cfg(test)]
mod popup_state_tests {
    use super::*;
    #[test]
    fn popup_canvas_is_explicit_bounded_single_and_terminal() {
        let mut state = State::default();
        state.popup_ready(320, 240);
        assert!(state.failed.is_some());
        for (width, height) in [(0, 240), (320, 0), (16385, 1), (16384, 16384)] {
            let mut state = State {
                popups: true,
                ..Default::default()
            };
            state.popup_ready(width, height);
            assert!(state.failed.is_some());
        }
        let mut state = State {
            popups: true,
            ..Default::default()
        };
        state.popup_ready(320, 240);
        assert_eq!(state.popup_layout, Some((320, 240)));
        state.popup_ready(320, 240);
        assert!(state.failed.is_some());
        let mut state = State {
            popups: true,
            ..Default::default()
        };
        state.fail("stopped");
        state.popup_ready(320, 240);
        assert!(state.popup_layout.is_none());
    }
}

fn require_resize_capability(version: u32, popups: bool) -> RemoteResult<()> {
    if version < if popups { 2 } else { 1 } {
        return Err(invalid(
            "Wayland popup resize-follow requires explicit native version 2 capability",
        )
        .into());
    }
    Ok(())
}
#[cfg(test)]
mod popup_follow_state_tests {
    use super::*;
    #[test]
    fn combined_mode_requires_explicit_v2_without_fallback() {
        assert!(require_resize_capability(1, false).is_ok());
        assert!(require_resize_capability(1, true).is_err());
        assert!(require_resize_capability(2, true).is_ok());
        assert!(require_resize_capability(0, false).is_err());
    }
    #[test]
    fn combined_mode_preserves_popup_scope_across_layout_epochs_and_terminal() {
        let mut state = State {
            resize_follow: true,
            popups: true,
            ..Default::default()
        };
        for (epoch, width, height) in [(1, 320, 240), (2, 640, 360), (3, 320, 240)] {
            state.follow_event(resize_manager::Event::Suspended { epoch });
            assert!(state.follow_suspended && state.session_stopped && state.popups);
            assert!(state.follow_layout.is_none());
            state.follow_event(resize_manager::Event::Layout {
                epoch,
                width,
                height,
            });
            assert_eq!(state.follow_layout, Some((width as u16, height as u16)));
            assert!(state.popups && state.failed.is_none());
        }
        state.follow_event(resize_manager::Event::Stopped);
        state.follow_event(resize_manager::Event::Suspended { epoch: 4 });
        assert!(state.failed.is_some());
        assert_eq!(state.follow_epoch, 3);
    }
}

#[derive(Debug, PartialEq, Eq)]
struct ActivityReadiness {
    native_failed: bool,
    read_native: bool,
    wake_failed: bool,
}
fn activity_readiness(native: PollFlags, wake: PollFlags) -> ActivityReadiness {
    let errors = PollFlags::POLLERR | PollFlags::POLLHUP | PollFlags::POLLNVAL;
    ActivityReadiness {
        native_failed: native.intersects(errors),
        read_native: native.contains(PollFlags::POLLIN),
        wake_failed: wake.intersects(errors),
    }
}
#[cfg(test)]
mod activity_wake_tests {
    use super::*;
    #[test]
    fn wake_only_never_reads_native_guard() {
        let ready = activity_readiness(PollFlags::empty(), PollFlags::POLLIN);
        assert!(!ready.read_native && !ready.native_failed && !ready.wake_failed);
    }
    #[test]
    fn simultaneous_native_data_is_not_skipped_by_wake() {
        let ready = activity_readiness(PollFlags::POLLIN, PollFlags::POLLIN);
        assert!(ready.read_native && !ready.native_failed);
        let ready = activity_readiness(PollFlags::POLLIN, PollFlags::POLLHUP);
        assert!(ready.read_native && ready.wake_failed);
    }
    #[test]
    fn native_error_precedes_any_wakeup() {
        for error in [PollFlags::POLLERR, PollFlags::POLLHUP, PollFlags::POLLNVAL] {
            assert!(activity_readiness(error | PollFlags::POLLIN, PollFlags::POLLIN).native_failed);
            assert!(activity_readiness(PollFlags::empty(), error).wake_failed);
        }
        assert!(!activity_readiness(PollFlags::POLLOUT, PollFlags::POLLIN).read_native);
    }
}
