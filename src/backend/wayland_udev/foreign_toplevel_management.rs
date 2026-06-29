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
}

impl ForeignToplevelMgmtState {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(ForeignToplevelMgmtInner {
                managers: Vec::new(),
                handles: HashMap::new(),
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

    pub fn remove_window(&self, win: WindowId) {
        let mut inner = self.inner.lock_safe();
        if let Some(handles) = inner.handles.remove(&win) {
            for h in handles {
                h.closed();
            }
        }
    }

    pub fn update_title(&self, win: WindowId, title: &str) {
        let inner = self.inner.lock_safe();
        if let Some(handles) = inner.handles.get(&win) {
            for h in handles {
                h.title(title.to_string());
                h.done();
            }
        }
    }

    pub fn update_app_id(&self, win: WindowId, app_id: &str) {
        let inner = self.inner.lock_safe();
        if let Some(handles) = inner.handles.get(&win) {
            for h in handles {
                h.app_id(app_id.to_string());
                h.done();
            }
        }
    }

    pub fn update_state(&self, win: WindowId, states: &[ToplevelState]) {
        let inner = self.inner.lock_safe();
        if let Some(handles) = inner.handles.get(&win) {
            let state_bytes: Vec<u8> = states
                .iter()
                .flat_map(|s| (*s as u32).to_ne_bytes())
                .collect();
            for h in handles {
                h.state(state_bytes.clone());
                h.done();
            }
        }
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
        handle.state(Vec::new());
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
            handle.state(Vec::new());
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
}
