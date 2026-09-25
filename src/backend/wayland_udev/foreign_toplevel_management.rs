/// wlr-foreign-toplevel-management-unstable-v1 protocol implementation.
///
/// Enables taskbars (Waybar, sfwbar, etc.) to list, activate, close, maximize,
/// minimize, and fullscreen windows.
use crate::sync_ext::MutexExt;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use log::{debug, info};

use smithay::output::Output;
use smithay::reexports::wayland_protocols_wlr::foreign_toplevel::v1::server::{
    zwlr_foreign_toplevel_handle_v1::{self, State as ToplevelState, ZwlrForeignToplevelHandleV1},
    zwlr_foreign_toplevel_manager_v1::{self, ZwlrForeignToplevelManagerV1},
};
use smithay::reexports::wayland_server::backend::ObjectId;
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};
use smithay::utils::{Logical, Rectangle};

use crate::backend::api::{BackendEvent, MaximizeAxes, NetWmAction};
use crate::backend::common_define::WindowId;
use crate::backend::wayland::state::JwmWaylandState;

// --- Types ---

pub struct ForeignToplevelManagerData;
unsafe impl Send for ForeignToplevelManagerData {}

pub struct ForeignToplevelHandleData {
    pub window_id: WindowId,
}
unsafe impl Send for ForeignToplevelHandleData {}

/// Shared state for foreign toplevel management.
#[derive(Clone)]
pub struct ForeignToplevelMgmtState {
    inner: Arc<Mutex<ForeignToplevelMgmtInner>>,
}

struct ForeignToplevelMgmtInner {
    managers: Vec<ZwlrForeignToplevelManagerV1>,
    handles: HashMap<WindowId, Vec<ZwlrForeignToplevelHandleV1>>,
    states: HashMap<WindowId, PublishedToplevelState>,
    /// The handle client's own wl_output objects each handle has been sent
    /// `output_enter` for, keyed by handle. `output_leave` may only name an
    /// output the handle entered, so this is the record the next sync diffs.
    entered_outputs: HashMap<ObjectId, EnteredOutputs>,
    /// Counts sync passes; a record the latest pass did not visit belongs to
    /// a handle that is gone.
    sync_pass: u64,
    /// Per-pass scratch, kept so a sync that finds nothing to send (nearly
    /// every event-loop turn) allocates nothing.
    output_rects: Vec<Option<Rectangle<i32, Logical>>>,
    overlapped: Vec<WlOutput>,
}

/// One handle's entered outputs and the sync pass that last visited it.
struct EnteredOutputs {
    outputs: Vec<WlOutput>,
    pass: u64,
}

/// How one handle's output membership moves from what it entered to what
/// the window now occupies.
#[derive(Debug, PartialEq, Eq)]
struct OutputMembershipPlan<T> {
    /// Everything the handle has entered once the plan is sent.
    target: Vec<T>,
    enter: Vec<T>,
    leave: Vec<T>,
}

/// Whether [`plan_output_membership`] would leave `entered` exactly as it
/// is: a target with the same members and nothing to send. `entered` never
/// holds duplicates, since it is a previous plan's target. Checked before
/// planning, so the steady state neither allocates nor rebuilds the record.
fn output_membership_unchanged<T: PartialEq>(
    entered: &[T],
    overlapped: Option<&[T]>,
    present: impl Fn(&T) -> bool,
) -> bool {
    let Some(overlapped) = overlapped else {
        // Parked on no output: the target is what was entered, minus what
        // is gone.
        return entered.iter().all(&present);
    };
    entered
        .iter()
        .all(|output| present(output) && overlapped.contains(output))
        && overlapped
            .iter()
            .all(|output| !present(output) || entered.contains(output))
}

/// Plan `output_enter`/`output_leave` for one handle.
///
/// `overlapped` holds the client's output objects for every output the
/// window intersects; it is empty when the client bound none of them. `None`
/// means the window intersects no output at all (parked while hidden or
/// minimized, or not placed yet): it keeps what it entered, because taskbars
/// such as waybar's wlr/taskbar drop the button on `output_leave` and a
/// minimized window must stay listed. `present` drops objects whose output is
/// gone or that the client destroyed; `alive` says whether a dropped object
/// can still be named in `output_leave`.
fn plan_output_membership<T: PartialEq + Clone>(
    entered: &[T],
    overlapped: Option<&[T]>,
    present: impl Fn(&T) -> bool,
    alive: impl Fn(&T) -> bool,
) -> OutputMembershipPlan<T> {
    let source = overlapped.unwrap_or(entered);
    let mut target: Vec<T> = Vec::with_capacity(source.len());
    for output in source {
        if present(output) && !target.contains(output) {
            target.push(output.clone());
        }
    }
    let enter = target
        .iter()
        .filter(|output| !entered.contains(output))
        .cloned()
        .collect();
    let leave = entered
        .iter()
        .filter(|output| !target.contains(output) && alive(output))
        .cloned()
        .collect();
    OutputMembershipPlan {
        target,
        enter,
        leave,
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct PublishedToplevelState {
    activated: bool,
    maximized_horz: bool,
    maximized_vert: bool,
    minimized: bool,
    fullscreen: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StateFlag {
    Activated,
    MaximizedHorz,
    MaximizedVert,
    Minimized,
    Fullscreen,
}

impl PublishedToplevelState {
    fn set(&mut self, flag: StateFlag, on: bool) -> bool {
        let value = match flag {
            StateFlag::Activated => &mut self.activated,
            StateFlag::MaximizedHorz => &mut self.maximized_horz,
            StateFlag::MaximizedVert => &mut self.maximized_vert,
            StateFlag::Minimized => &mut self.minimized,
            StateFlag::Fullscreen => &mut self.fullscreen,
        };
        if *value == on {
            return false;
        }
        *value = on;
        true
    }

    fn protocol_states(self) -> Vec<ToplevelState> {
        let mut states = Vec::with_capacity(4);
        // wlr has one maximized bit, whereas EWMH lets JWM track each axis.
        // Only the full two-axis state is an xdg/wlr-style maximization.
        if self.maximized_horz && self.maximized_vert {
            states.push(ToplevelState::Maximized);
        }
        if self.minimized {
            states.push(ToplevelState::Minimized);
        }
        if self.activated {
            states.push(ToplevelState::Activated);
        }
        if self.fullscreen {
            states.push(ToplevelState::Fullscreen);
        }
        states
    }

    fn protocol_state_bytes(self) -> Vec<u8> {
        encode_states(&self.protocol_states())
    }
}

fn encode_states(states: &[ToplevelState]) -> Vec<u8> {
    states
        .iter()
        .flat_map(|state| (*state as u32).to_ne_bytes())
        .collect()
}

impl ForeignToplevelMgmtState {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(ForeignToplevelMgmtInner {
                managers: Vec::new(),
                handles: HashMap::new(),
                states: HashMap::new(),
                entered_outputs: HashMap::new(),
                sync_pass: 0,
                output_rects: Vec::new(),
                overlapped: Vec::new(),
            })),
        }
    }

    pub fn add_manager(&self, mgr: ZwlrForeignToplevelManagerV1) {
        self.inner.lock_safe().managers.push(mgr);
    }

    pub fn remove_manager(&self, mgr: &ZwlrForeignToplevelManagerV1) {
        let mut inner = self.inner.lock_safe();
        inner.managers.retain(|m| m != mgr);
    }

    pub fn add_handle(&self, win: WindowId, handle: ZwlrForeignToplevelHandleV1) {
        self.inner
            .lock_safe()
            .handles
            .entry(win)
            .or_default()
            .push(handle);
    }

    pub fn remove_handle(&self, win: WindowId, handle: &ZwlrForeignToplevelHandleV1) {
        let mut inner = self.inner.lock_safe();
        let Some(handles) = inner.handles.get_mut(&win) else {
            return;
        };
        handles.retain(|candidate| candidate != handle && candidate.is_alive());
        if handles.is_empty() {
            inner.handles.remove(&win);
        }
        inner.entered_outputs.remove(&handle.id());
    }

    pub fn add_window(&self, win: WindowId) {
        self.inner.lock_safe().states.entry(win).or_default();
    }

    pub fn remove_window(&self, win: WindowId) {
        let mut inner = self.inner.lock_safe();
        inner.states.remove(&win);
        if let Some(handles) = inner.handles.remove(&win) {
            for h in handles {
                inner.entered_outputs.remove(&h.id());
                h.closed();
            }
        }
    }

    pub fn update_title(&self, win: WindowId, title: &str) {
        let mut inner = self.inner.lock_safe();
        if let Some(handles) = inner.handles.get_mut(&win) {
            handles.retain(Resource::is_alive);
            for h in handles {
                h.title(title.to_string());
                h.done();
            }
        }
    }

    pub fn update_app_id(&self, win: WindowId, app_id: &str) {
        let mut inner = self.inner.lock_safe();
        if let Some(handles) = inner.handles.get_mut(&win) {
            handles.retain(Resource::is_alive);
            for h in handles {
                h.app_id(app_id.to_string());
                h.done();
            }
        }
    }

    pub(crate) fn update_state(&self, win: WindowId, flag: StateFlag, on: bool) {
        let (handles, state_bytes) = {
            let mut inner = self.inner.lock_safe();
            let Some(state) = inner.states.get_mut(&win) else {
                return;
            };
            let old_protocol_states = state.protocol_states();
            if !state.set(flag, on) {
                return;
            }
            let new_protocol_states = state.protocol_states();
            if old_protocol_states == new_protocol_states {
                return;
            }
            let state_bytes = encode_states(&new_protocol_states);
            let handles = inner.handles.entry(win).or_default();
            handles.retain(Resource::is_alive);
            let handles = handles.clone();
            (handles, state_bytes)
        };

        for handle in handles {
            handle.state(state_bytes.clone());
            handle.done();
        }
    }

    fn state_bytes(&self, win: WindowId) -> Vec<u8> {
        self.inner
            .lock_safe()
            .states
            .get(&win)
            .copied()
            .unwrap_or_default()
            .protocol_state_bytes()
    }

    pub fn managers(&self) -> Vec<ZwlrForeignToplevelManagerV1> {
        self.inner.lock_safe().managers.clone()
    }

    /// Every window currently published to managers, in a stable order.
    fn windows(&self) -> Vec<WindowId> {
        let mut windows: Vec<WindowId> = self.inner.lock_safe().states.keys().copied().collect();
        windows.sort_by_key(|window| window.raw());
        windows
    }

    /// Bring every live handle's `output_enter`/`output_leave` in line with
    /// the outputs its window intersects, followed by `done` when anything
    /// changed. Returns whether any event was queued.
    ///
    /// Run once per event-loop turn instead of at each geometry write: the
    /// window geometry has many writers, and the handle's client may bind a
    /// wl_output (or an output may be hot-plugged) after the handle exists,
    /// which only a diff against the entered record catches. Since nearly
    /// every turn finds nothing to send, the pass works in place: it reuses
    /// its scratch buffers and replaces a handle's record only when the
    /// membership changed.
    fn sync_outputs(
        &self,
        outputs: &[Output],
        window_rect: impl Fn(WindowId) -> Option<Rectangle<i32, Logical>>,
    ) -> bool {
        let mut guard = self.inner.lock_safe();
        let inner = &mut *guard;
        if inner.handles.is_empty() {
            inner.entered_outputs.clear();
            return false;
        }
        inner.sync_pass = inner.sync_pass.wrapping_add(1);
        let pass = inner.sync_pass;
        // Global layout rectangles in the same physical space as
        // `window_geometry`: the output origin and its unscaled mode size.
        // An output without a mode is not laid out, so nothing is on it.
        inner.output_rects.clear();
        inner.output_rects.extend(outputs.iter().map(|output| {
            let mode = output.current_mode()?;
            Some(Rectangle::new(
                output.current_location(),
                (mode.size.w, mode.size.h).into(),
            ))
        }));
        let output_rects = &inner.output_rects;
        let present = |wl_output: &WlOutput| {
            wl_output.is_alive()
                && Output::from_resource(wl_output).is_some_and(|output| {
                    outputs
                        .iter()
                        .zip(output_rects)
                        .any(|(live, rect)| rect.is_some() && *live == output)
                })
        };

        let mut sent = false;
        for (win, handles) in &inner.handles {
            let rect = window_rect(*win);
            for handle in handles.iter().filter(|handle| handle.is_alive()) {
                let Some(client) = handle.client() else {
                    continue;
                };
                let overlapped = &mut inner.overlapped;
                overlapped.clear();
                let mut on_an_output = false;
                if let Some(rect) = rect {
                    for (output, output_rect) in outputs.iter().zip(output_rects) {
                        if output_rect.is_some_and(|output_rect| output_rect.overlaps(rect)) {
                            on_an_output = true;
                            overlapped.extend(output.client_outputs(&client));
                        }
                    }
                }
                let overlapped = on_an_output.then_some(overlapped.as_slice());
                let entered =
                    inner
                        .entered_outputs
                        .entry(handle.id())
                        .or_insert_with(|| EnteredOutputs {
                            outputs: Vec::new(),
                            pass,
                        });
                entered.pass = pass;
                if output_membership_unchanged(&entered.outputs, overlapped, present) {
                    continue;
                }
                let plan = plan_output_membership(
                    &entered.outputs,
                    overlapped,
                    present,
                    Resource::is_alive,
                );
                for wl_output in &plan.leave {
                    handle.output_leave(wl_output);
                }
                for wl_output in &plan.enter {
                    handle.output_enter(wl_output);
                }
                if !plan.enter.is_empty() || !plan.leave.is_empty() {
                    handle.done();
                    sent = true;
                }
                entered.outputs = plan.target;
            }
        }
        // Records of handles that died or lost their client since.
        inner
            .entered_outputs
            .retain(|_, entered| entered.pass == pass);
        sent
    }
}

impl JwmWaylandState {
    /// Keep every wlr foreign-toplevel handle's output membership in step
    /// with window geometry, outputs and the handle clients' wl_output binds.
    /// Output-filtered taskbars (waybar's wlr/taskbar) show a window only
    /// after its handle entered one of their outputs. Returns whether any
    /// event was queued for the clients.
    pub(crate) fn sync_foreign_toplevel_outputs(&mut self) -> bool {
        let Some(ftm) = self.foreign_toplevel_mgmt.as_ref() else {
            return false;
        };
        let geometry = &self.window_geometry;
        ftm.sync_outputs(&self.outputs, |win| {
            geometry
                .get(&win)
                .map(|g| Rectangle::new((g.x, g.y).into(), (g.w as i32, g.h as i32).into()))
        })
    }
}

/// Initialize the wlr-foreign-toplevel-manager global.
pub fn init_foreign_toplevel_management(dh: &DisplayHandle) -> ForeignToplevelMgmtState {
    dh.create_global::<JwmWaylandState, ZwlrForeignToplevelManagerV1, _>(
        3,
        ForeignToplevelManagerData,
    );
    info!("[udev/wayland] zwlr-foreign-toplevel-management-unstable-v1 global registered");
    ForeignToplevelMgmtState::new()
}

/// Announce a new toplevel to all bound managers.
pub fn announce_new_toplevel(
    dh: &DisplayHandle,
    ftm: &ForeignToplevelMgmtState,
    win_id: WindowId,
    title: &str,
    app_id: &str,
) {
    ftm.add_window(win_id);
    let state_bytes = ftm.state_bytes(win_id);
    let managers = ftm.managers();
    for mgr in &managers {
        let Some(client) = mgr.client() else { continue };
        let Ok(handle) = client.create_resource::<ZwlrForeignToplevelHandleV1, _, JwmWaylandState>(
            dh,
            mgr.version(),
            ForeignToplevelHandleData { window_id: win_id },
        ) else {
            continue;
        };

        mgr.toplevel(&handle);
        handle.title(title.to_string());
        handle.app_id(app_id.to_string());
        handle.state(state_bytes.clone());
        handle.done();

        ftm.add_handle(win_id, handle);
    }
}

// --- GlobalDispatch for the manager ---

impl GlobalDispatch<ZwlrForeignToplevelManagerV1, ForeignToplevelManagerData> for JwmWaylandState {
    fn bind(
        state: &mut Self,
        dh: &DisplayHandle,
        client: &Client,
        resource: New<ZwlrForeignToplevelManagerV1>,
        _global_data: &ForeignToplevelManagerData,
        data_init: &mut DataInit<'_, Self>,
    ) {
        state.record_protocol_bind("zwlr_foreign_toplevel_manager_v1");
        let mgr = data_init.init(resource, ForeignToplevelManagerData);

        // Send existing windows to the newly-bound manager: every window
        // announced to managers, which is exactly the set `closed` will later
        // be sent for.
        let published = state
            .foreign_toplevel_mgmt
            .as_ref()
            .map(ForeignToplevelMgmtState::windows)
            .unwrap_or_default();
        for win_id in published {
            let title = state.window_title.get(&win_id).cloned().unwrap_or_default();
            let app_id = state
                .window_app_id
                .get(&win_id)
                .cloned()
                .unwrap_or_default();

            let Ok(handle) = client.create_resource::<ZwlrForeignToplevelHandleV1, _, Self>(
                dh,
                mgr.version(),
                ForeignToplevelHandleData { window_id: win_id },
            ) else {
                continue;
            };

            mgr.toplevel(&handle);
            handle.title(title);
            handle.app_id(app_id);
            let state_bytes = state
                .foreign_toplevel_mgmt
                .as_ref()
                .map(|ftm| ftm.state_bytes(win_id))
                .unwrap_or_default();
            handle.state(state_bytes);
            handle.done();

            if let Some(ref ftm) = state.foreign_toplevel_mgmt {
                ftm.add_handle(win_id, handle);
            }
        }

        if let Some(ref ftm) = state.foreign_toplevel_mgmt {
            ftm.add_manager(mgr);
        }
    }

    /// Window control is privileged: a sandboxed (wp_security_context)
    /// client must not list, activate or close other clients' windows.
    fn can_view(client: Client, _global_data: &ForeignToplevelManagerData) -> bool {
        !crate::backend::wayland::state::client_is_sandboxed(&client)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ForeignToplevelMgmtState, OutputMembershipPlan, PublishedToplevelState, StateFlag,
        ToplevelState, announce_new_toplevel, encode_states, output_membership_unchanged,
        plan_output_membership,
    };
    use crate::backend::api::{BackendEvent, Geometry};
    use crate::backend::common_define::WindowId;
    use crate::backend::wayland::state::{JwmClientState, JwmWaylandState};
    use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
    use smithay::reexports::calloop::{EventLoop, channel};
    use smithay::reexports::wayland_server::{Display, DisplayHandle, Resource};
    use std::collections::VecDeque;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};

    /// zwlr_foreign_toplevel_manager_v1.toplevel
    const MANAGER_TOPLEVEL: u16 = 0;
    /// zwlr_foreign_toplevel_handle_v1.output_enter / output_leave / done
    const HANDLE_OUTPUT_ENTER: u16 = 2;
    const HANDLE_OUTPUT_LEAVE: u16 = 3;
    const HANDLE_DONE: u16 = 5;
    const HANDLE_CLOSED: u16 = 6;

    type Frame = (u32, u16, Vec<u8>);

    fn wire_message(sender: u32, opcode: u16, payload: &[u8]) -> Vec<u8> {
        let size = 8 + payload.len();
        let mut message = Vec::with_capacity(size);
        message.extend_from_slice(&sender.to_ne_bytes());
        message.extend_from_slice(&(((size as u32) << 16) | u32::from(opcode)).to_ne_bytes());
        message.extend_from_slice(payload);
        message
    }

    fn wire_bind(
        registry: u32,
        registry_name: u32,
        object_id: u32,
        interface: &str,
        version: u32,
    ) -> Vec<u8> {
        let len = interface.len() + 1;
        let mut payload = Vec::new();
        payload.extend_from_slice(&registry_name.to_ne_bytes());
        payload.extend_from_slice(&(len as u32).to_ne_bytes());
        payload.extend_from_slice(interface.as_bytes());
        payload.push(0);
        payload.resize((payload.len() + 3) & !3, 0);
        payload.extend_from_slice(&version.to_ne_bytes());
        payload.extend_from_slice(&object_id.to_ne_bytes());
        // wl_registry.bind
        wire_message(registry, 0, &payload)
    }

    fn read_u32(bytes: &[u8], offset: usize) -> u32 {
        u32::from_ne_bytes(bytes[offset..offset + 4].try_into().expect("wire u32"))
    }

    /// A raw Wayland client talking to a headless `JwmWaylandState`. Fields
    /// drop in declaration order: the client first, the event loop last.
    struct WireClient {
        peer: UnixStream,
        state: JwmWaylandState,
        display: Display<JwmWaylandState>,
        _event_loop: EventLoop<'static, JwmWaylandState>,
        /// Client object ids are allocated densely, as the server requires.
        next_id: u32,
    }

    impl WireClient {
        fn new() -> Self {
            let event_loop: EventLoop<'static, JwmWaylandState> =
                EventLoop::try_new().expect("create test event loop");
            let display = Display::<JwmWaylandState>::new().expect("create test display");
            let mut display_handle = display.handle();
            let (flush_tx, _flush_rx) = channel::channel();
            let (state, socket_name) = JwmWaylandState::init(
                &display_handle,
                event_loop.handle(),
                Arc::new(Mutex::new(VecDeque::<BackendEvent>::new())),
                flush_tx,
                Arc::new(AtomicBool::new(false)),
                "ftm-output-seat".to_owned(),
                false,
                false,
            )
            .expect("initialize headless Wayland state");
            assert!(socket_name.is_none());
            let (server, peer) = UnixStream::pair().expect("create Wayland socket pair");
            peer.set_read_timeout(Some(std::time::Duration::from_secs(2)))
                .expect("set wire read timeout");
            display_handle
                .insert_client(server, Arc::new(JwmClientState::default()))
                .expect("insert raw Wayland client");
            Self {
                peer,
                state,
                display,
                _event_loop: event_loop,
                // wl_display is object 1.
                next_id: 2,
            }
        }

        fn handle(&self) -> DisplayHandle {
            self.display.handle()
        }

        fn new_id(&mut self) -> u32 {
            let id = self.next_id;
            self.next_id += 1;
            id
        }

        /// Send `requests` plus a `wl_display.sync`, dispatch them, flush the
        /// server's queue and return every event up to the sync's callback.
        fn roundtrip(&mut self, requests: &[u8]) -> Vec<Frame> {
            let callback = self.new_id();
            let mut bytes = requests.to_vec();
            bytes.extend_from_slice(&wire_message(1, 0, &callback.to_ne_bytes()));
            self.peer.write_all(&bytes).expect("send requests");
            self.display
                .dispatch_clients(&mut self.state)
                .expect("dispatch client requests");
            self.display.flush_clients().expect("flush Wayland events");

            let mut buffer = Vec::new();
            let mut frames = Vec::new();
            let mut offset = 0;
            loop {
                let mut chunk = [0u8; 8192];
                let read = self.peer.read(&mut chunk).expect("read Wayland events");
                assert!(read > 0, "Wayland server closed before the sync callback");
                buffer.extend_from_slice(&chunk[..read]);
                while buffer.len().saturating_sub(offset) >= 8 {
                    let sender = read_u32(&buffer, offset);
                    let header = read_u32(&buffer, offset + 4);
                    let size = (header >> 16) as usize;
                    assert!(size >= 8, "malformed Wayland event header");
                    if buffer.len() - offset < size {
                        break;
                    }
                    let opcode = header as u16;
                    frames.push((sender, opcode, buffer[offset + 8..offset + size].to_vec()));
                    offset += size;
                    if sender == callback && opcode == 0 {
                        return frames;
                    }
                }
            }
        }
    }

    /// Registry globals from the events of the roundtrip that created
    /// `registry`: (name, interface, version).
    fn registry_globals(frames: &[Frame], registry: u32) -> Vec<(u32, String, u32)> {
        frames
            .iter()
            .filter(|(sender, opcode, _)| *sender == registry && *opcode == 0)
            .map(|(_, _, payload)| {
                let name = read_u32(payload, 0);
                let len = read_u32(payload, 4) as usize;
                let interface = std::str::from_utf8(&payload[8..8 + len.saturating_sub(1)])
                    .expect("registry interface is UTF-8")
                    .to_owned();
                let version = read_u32(payload, 8 + ((len + 3) & !3));
                (name, interface, version)
            })
            .collect()
    }

    fn test_output(display: &DisplayHandle, name: &str, location: (i32, i32)) -> Output {
        let output = Output::new(
            name.to_owned(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "jwm".into(),
                model: "test".into(),
                serial_number: name.into(),
            },
        );
        let mode = Mode {
            size: (1920, 1080).into(),
            refresh: 60_000,
        };
        output.add_mode(mode);
        output.set_preferred(mode);
        output.change_current_state(Some(mode), None, None, Some(location.into()));
        output.create_global::<JwmWaylandState>(display);
        output
    }

    /// (sender, opcode, first payload word) of every event `handle` received.
    fn handle_events(frames: &[Frame], handle: u32) -> Vec<(u16, Option<u32>)> {
        frames
            .iter()
            .filter(|(sender, _, _)| *sender == handle)
            .map(|(_, opcode, payload)| {
                (*opcode, (payload.len() >= 4).then(|| read_u32(payload, 0)))
            })
            .collect()
    }

    #[test]
    fn membership_follows_the_window_and_sticks_while_it_is_parked() {
        let always = |_: &u32| true;
        let plan = plan_output_membership(&[], Some(&[4, 4]), always, always);
        assert_eq!(
            plan,
            OutputMembershipPlan {
                target: vec![4],
                enter: vec![4],
                leave: vec![],
            }
        );

        // Moving onto an output the client never bound leaves the old one.
        let plan = plan_output_membership(&[4], Some(&[]), always, always);
        assert_eq!(
            (plan.target, plan.enter, plan.leave),
            (vec![], vec![], vec![4])
        );

        // A window on no output at all keeps what it entered.
        let plan = plan_output_membership(&[4, 6], None, always, always);
        assert_eq!(
            (plan.target, plan.enter, plan.leave),
            (vec![4, 6], vec![], vec![])
        );

        // A destroyed object is dropped, and never named in output_leave.
        let live = |id: &u32| *id != 6;
        let plan = plan_output_membership(&[4, 6], None, live, live);
        assert_eq!(
            (plan.target, plan.enter, plan.leave),
            (vec![4], vec![], vec![])
        );
        let plan = plan_output_membership(&[4, 6], Some(&[8]), live, live);
        assert_eq!(
            (plan.target, plan.enter, plan.leave),
            (vec![8], vec![8], vec![4])
        );
    }

    /// The fast path must agree with the plan it skips: it may only say
    /// "unchanged" when planning would send nothing and keep the same
    /// members, and it must say so whenever that is the case, or the record
    /// would be rebuilt on every loop turn.
    #[test]
    fn the_unchanged_membership_check_agrees_with_the_plan() {
        let entered_sets: [&[u32]; 5] = [&[], &[4], &[6], &[4, 6], &[6, 4]];
        let overlapped_sets: [Option<&[u32]>; 7] = [
            None,
            Some(&[]),
            Some(&[4]),
            Some(&[4, 4]),
            Some(&[6, 4]),
            Some(&[4, 6, 8]),
            Some(&[8]),
        ];
        let everything = |_: &u32| true;
        let not_six = |id: &u32| *id != 6;
        let not_eight = |id: &u32| *id != 8;
        let sorted = |outputs: &[u32]| {
            let mut outputs = outputs.to_vec();
            outputs.sort_unstable();
            outputs
        };
        for entered in entered_sets {
            for overlapped in overlapped_sets {
                for present in [&everything as &dyn Fn(&u32) -> bool, &not_six, &not_eight] {
                    let plan = plan_output_membership(entered, overlapped, present, everything);
                    let unchanged = plan.enter.is_empty()
                        && plan.leave.is_empty()
                        && sorted(&plan.target) == sorted(entered);
                    assert_eq!(
                        output_membership_unchanged(entered, overlapped, present),
                        unchanged,
                        "entered {entered:?}, overlapped {overlapped:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn handles_enter_the_client_outputs_their_window_occupies() {
        let mut client = WireClient::new();
        let display = client.handle();
        client.state.outputs = vec![
            test_output(&display, "TEST-L", (0, 0)),
            test_output(&display, "TEST-R", (1920, 0)),
        ];

        // wl_display.get_registry
        let registry = client.new_id();
        let frames = client.roundtrip(&wire_message(1, 1, &registry.to_ne_bytes()));
        let globals = registry_globals(&frames, registry);
        let mut output_globals: Vec<u32> = globals
            .iter()
            .filter(|(_, interface, _)| interface == "wl_output")
            .map(|(name, _, _)| *name)
            .collect();
        output_globals.sort_unstable();
        let [left_global, right_global] = output_globals[..] else {
            panic!("expected the two test wl_output globals, got {output_globals:?}");
        };
        let (manager_global, manager_version) = globals
            .iter()
            .find(|(_, interface, _)| interface == "zwlr_foreign_toplevel_manager_v1")
            .map(|(name, _, version)| (*name, *version))
            .expect("foreign toplevel manager global");

        // A taskbar binds its manager and, for now, only the left output.
        let left_output = client.new_id();
        let manager = client.new_id();
        let mut requests = wire_bind(registry, left_global, left_output, "wl_output", 4);
        requests.extend_from_slice(&wire_bind(
            registry,
            manager_global,
            manager,
            "zwlr_foreign_toplevel_manager_v1",
            manager_version.min(3),
        ));
        let frames = client.roundtrip(&requests);
        let left_geometry = frames
            .iter()
            .find(|(sender, opcode, _)| *sender == left_output && *opcode == 0)
            .expect("wl_output.geometry");
        assert_eq!(
            read_u32(&left_geometry.2, 0),
            0,
            "the first global is the left output"
        );

        let window = WindowId::from_raw(7);
        let ftm = client
            .state
            .foreign_toplevel_mgmt
            .clone()
            .expect("foreign toplevel management is enabled by default");
        announce_new_toplevel(&display, &ftm, window, "title", "app");
        client.state.window_geometry.insert(
            window,
            Geometry {
                x: 100,
                y: 100,
                w: 800,
                h: 600,
                border: 0,
            },
        );
        assert!(client.state.sync_foreign_toplevel_outputs());
        // The pass runs on every loop turn: when nothing changed it must
        // leave the handle's record where it is instead of rebuilding it.
        let entered_record = || {
            let inner = ftm.inner.lock().expect("foreign toplevel state");
            let handle = inner.handles[&window][0].id();
            let entered = &inner.entered_outputs[&handle].outputs;
            (entered.as_ptr(), entered.len())
        };
        let before = entered_record();
        assert!(
            !client.state.sync_foreign_toplevel_outputs(),
            "an unchanged window sends nothing"
        );
        assert_eq!(
            entered_record(),
            before,
            "an unchanged window keeps its record"
        );
        let frames = client.roundtrip(&[]);
        let handle = frames
            .iter()
            .find(|(sender, opcode, _)| *sender == manager && *opcode == MANAGER_TOPLEVEL)
            .map(|(_, _, payload)| read_u32(payload, 0))
            .expect("manager announces the toplevel");
        let events = handle_events(&frames, handle);
        let enter_at = events
            .iter()
            .position(|event| *event == (HANDLE_OUTPUT_ENTER, Some(left_output)))
            .expect("the handle enters the client's left wl_output");
        assert_eq!(events.last(), Some(&(HANDLE_DONE, None)));
        assert!(enter_at < events.len() - 1, "done follows the output_enter");

        // Onto the right output, which this client has not bound yet.
        if let Some(geometry) = client.state.window_geometry.get_mut(&window) {
            geometry.x = 2000;
        }
        assert!(client.state.sync_foreign_toplevel_outputs());
        let frames = client.roundtrip(&[]);
        assert_eq!(
            handle_events(&frames, handle),
            vec![
                (HANDLE_OUTPUT_LEAVE, Some(left_output)),
                (HANDLE_DONE, None)
            ]
        );

        // Binding it afterwards still delivers the enter.
        let right_output = client.new_id();
        client.roundtrip(&wire_bind(
            registry,
            right_global,
            right_output,
            "wl_output",
            4,
        ));
        assert!(client.state.sync_foreign_toplevel_outputs());
        let frames = client.roundtrip(&[]);
        assert_eq!(
            handle_events(&frames, handle),
            vec![
                (HANDLE_OUTPUT_ENTER, Some(right_output)),
                (HANDLE_DONE, None)
            ]
        );

        // Parked off every output (hidden or minimized): the taskbar keeps it.
        if let Some(geometry) = client.state.window_geometry.get_mut(&window) {
            geometry.x = -10_000;
        }
        assert!(!client.state.sync_foreign_toplevel_outputs());

        ftm.remove_window(window);
        assert!(!client.state.sync_foreign_toplevel_outputs());
        let frames = client.roundtrip(&[]);
        assert_eq!(handle_events(&frames, handle), vec![(HANDLE_CLOSED, None)]);
    }

    #[test]
    fn published_state_contains_every_wlr_observable_flag() {
        let mut state = PublishedToplevelState::default();
        assert!(state.protocol_states().is_empty());

        assert!(state.set(StateFlag::Activated, true));
        assert!(state.set(StateFlag::MaximizedVert, true));
        assert!(state.set(StateFlag::MaximizedHorz, true));
        assert!(state.set(StateFlag::Minimized, true));
        assert!(state.set(StateFlag::Fullscreen, true));

        assert_eq!(
            state.protocol_states(),
            vec![
                ToplevelState::Maximized,
                ToplevelState::Minimized,
                ToplevelState::Activated,
                ToplevelState::Fullscreen,
            ]
        );
    }

    #[test]
    fn maximized_requires_both_ewmh_axes() {
        let mut state = PublishedToplevelState::default();
        state.set(StateFlag::MaximizedVert, true);
        assert!(!state.protocol_states().contains(&ToplevelState::Maximized));

        state.set(StateFlag::MaximizedHorz, true);
        assert!(state.protocol_states().contains(&ToplevelState::Maximized));

        state.set(StateFlag::MaximizedVert, false);
        assert!(!state.protocol_states().contains(&ToplevelState::Maximized));
    }

    #[test]
    fn setting_an_unchanged_flag_is_a_noop() {
        let mut state = PublishedToplevelState::default();
        assert!(state.set(StateFlag::Minimized, true));
        assert!(!state.set(StateFlag::Minimized, true));
        assert!(state.set(StateFlag::Minimized, false));
        assert!(!state.set(StateFlag::Minimized, false));
    }

    #[test]
    fn state_is_cached_before_any_manager_binds() {
        let manager = ForeignToplevelMgmtState::new();
        let window = WindowId::from_raw(42);
        manager.add_window(window);
        manager.update_state(window, StateFlag::Minimized, true);
        manager.update_state(window, StateFlag::Fullscreen, true);

        assert_eq!(
            manager.state_bytes(window),
            encode_states(&[ToplevelState::Minimized, ToplevelState::Fullscreen])
        );
    }
}

// --- Dispatch for the manager ---

impl Dispatch<ZwlrForeignToplevelManagerV1, ForeignToplevelManagerData> for JwmWaylandState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &ZwlrForeignToplevelManagerV1,
        request: zwlr_foreign_toplevel_manager_v1::Request,
        _data: &ForeignToplevelManagerData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwlr_foreign_toplevel_manager_v1::Request::Stop => {
                resource.finished();
                if let Some(ref ftm) = state.foreign_toplevel_mgmt {
                    ftm.remove_manager(resource);
                }
            }
            _ => {}
        }
    }

    fn destroyed(
        state: &mut Self,
        _client: smithay::reexports::wayland_server::backend::ClientId,
        resource: &ZwlrForeignToplevelManagerV1,
        _data: &ForeignToplevelManagerData,
    ) {
        if let Some(ref ftm) = state.foreign_toplevel_mgmt {
            ftm.remove_manager(resource);
        }
    }
}

// --- Dispatch for toplevel handles ---

impl Dispatch<ZwlrForeignToplevelHandleV1, ForeignToplevelHandleData> for JwmWaylandState {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &ZwlrForeignToplevelHandleV1,
        request: zwlr_foreign_toplevel_handle_v1::Request,
        data: &ForeignToplevelHandleData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        let win = data.window_id;
        match request {
            zwlr_foreign_toplevel_handle_v1::Request::Activate { seat: _ } => {
                debug!("[foreign-toplevel] activate request for {:?}", win);
                state.push_event(BackendEvent::ForeignToplevelActivate(win));
            }
            zwlr_foreign_toplevel_handle_v1::Request::Close => {
                debug!("[foreign-toplevel] close request for {:?}", win);
                state.push_event(BackendEvent::ForeignToplevelClose(win));
            }
            // wlr has one maximized bit, so a taskbar always names both
            // axes. Shared policy decides and publishes the result.
            zwlr_foreign_toplevel_handle_v1::Request::SetMaximized => {
                debug!("[foreign-toplevel] set_maximized for {:?}", win);
                state.push_event(BackendEvent::WindowMaximizeRequest {
                    window: win,
                    action: NetWmAction::Add,
                    axes: MaximizeAxes::BOTH,
                });
            }
            zwlr_foreign_toplevel_handle_v1::Request::UnsetMaximized => {
                debug!("[foreign-toplevel] unset_maximized for {:?}", win);
                state.push_event(BackendEvent::WindowMaximizeRequest {
                    window: win,
                    action: NetWmAction::Remove,
                    axes: MaximizeAxes::BOTH,
                });
            }
            zwlr_foreign_toplevel_handle_v1::Request::SetMinimized => {
                debug!("[foreign-toplevel] set_minimized for {:?}", win);
                state.push_event(BackendEvent::ForeignToplevelSetMinimized(win, true));
            }
            zwlr_foreign_toplevel_handle_v1::Request::UnsetMinimized => {
                debug!("[foreign-toplevel] unset_minimized for {:?}", win);
                state.push_event(BackendEvent::ForeignToplevelSetMinimized(win, false));
            }
            zwlr_foreign_toplevel_handle_v1::Request::SetFullscreen { output: _ } => {
                debug!("[foreign-toplevel] set_fullscreen for {:?}", win);
                state.push_event(BackendEvent::ForeignToplevelSetFullscreen(win, true));
            }
            zwlr_foreign_toplevel_handle_v1::Request::UnsetFullscreen => {
                debug!("[foreign-toplevel] unset_fullscreen for {:?}", win);
                state.push_event(BackendEvent::ForeignToplevelSetFullscreen(win, false));
            }
            zwlr_foreign_toplevel_handle_v1::Request::SetRectangle { .. } => {}
            zwlr_foreign_toplevel_handle_v1::Request::Destroy => {}
            _ => {}
        }
    }

    fn destroyed(
        state: &mut Self,
        _client: smithay::reexports::wayland_server::backend::ClientId,
        resource: &ZwlrForeignToplevelHandleV1,
        data: &ForeignToplevelHandleData,
    ) {
        if let Some(ref ftm) = state.foreign_toplevel_mgmt {
            ftm.remove_handle(data.window_id, resource);
        }
    }
}
