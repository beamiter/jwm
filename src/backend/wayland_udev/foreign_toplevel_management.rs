/// wlr-foreign-toplevel-management-unstable-v1 protocol implementation.
///
/// Enables taskbars (Waybar, sfwbar, etc.) to list, activate, close, maximize,
/// minimize, and fullscreen windows.
use crate::sync_ext::MutexExt;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use log::{debug, info};

use smithay::reexports::wayland_protocols_wlr::foreign_toplevel::v1::server::{
    zwlr_foreign_toplevel_handle_v1::{self, State as ToplevelState, ZwlrForeignToplevelHandleV1},
    zwlr_foreign_toplevel_manager_v1::{self, ZwlrForeignToplevelManagerV1},
};
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};

use crate::backend::api::BackendEvent;
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
    }

    pub fn add_window(&self, win: WindowId) {
        self.inner.lock_safe().states.entry(win).or_default();
    }

    pub fn remove_window(&self, win: WindowId) {
        let mut inner = self.inner.lock_safe();
        inner.states.remove(&win);
        if let Some(handles) = inner.handles.remove(&win) {
            for h in handles {
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

        // Send existing windows to the newly-bound manager.
        for (&win_id, _) in &state.toplevels {
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
}

#[cfg(test)]
mod tests {
    use super::{
        ForeignToplevelMgmtState, PublishedToplevelState, StateFlag, ToplevelState, encode_states,
    };
    use crate::backend::common_define::WindowId;

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
            zwlr_foreign_toplevel_handle_v1::Request::SetMaximized => {
                debug!("[foreign-toplevel] set_maximized for {:?}", win);
                state.push_event(BackendEvent::ForeignToplevelSetMaximized(win, true));
            }
            zwlr_foreign_toplevel_handle_v1::Request::UnsetMaximized => {
                debug!("[foreign-toplevel] unset_maximized for {:?}", win);
                state.push_event(BackendEvent::ForeignToplevelSetMaximized(win, false));
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
