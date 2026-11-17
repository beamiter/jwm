//! Resize-only grants bound to a live local IPC connection and ClientKey.
//! The host retains its X11 challenge property for the whole lease. Native
//! submission atomically checks that proof and lifecycle history. Expected
//! size is only a precondition, not an epoch/ABA guarantee; the remote host
//! independently fences source/geometry epochs and confirms actual pixels.
//! This does not isolate clients from a malicious local X11 client.
use super::Jwm;
use crate::backend::api::{Backend, Geometry, WindowHandoffIdentity};
use crate::core::models::ClientKey;
use crate::ipc::IpcResponse;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;

#[derive(Clone, Copy)]
struct Grant {
    id: u64,
    key: ClientKey,
    challenge: [u8; 32],
}
#[derive(Default)]
pub(crate) struct RemoteResizeGrants {
    next: u64,
    owners: HashMap<u64, Grant>,
}
impl RemoteResizeGrants {
    pub(crate) fn retain_live(&mut self, mut live: impl FnMut(u64) -> bool) {
        self.owners.retain(|owner, _| live(*owner));
    }
    pub(crate) fn revoke_client(&mut self, key: ClientKey) {
        self.owners.retain(|_, grant| grant.key != key);
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GrantArgs {
    window: u32,
    challenge: [u8; 32],
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ApplyArgs {
    grant: u64,
    expected_width: u16,
    expected_height: u16,
    width: u16,
    height: u16,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RevokeArgs {
    grant: u64,
}

pub(crate) fn is_command(name: &str) -> bool {
    matches!(
        name,
        "remote_resize_grant" | "remote_resize_apply" | "remote_resize_revoke"
    )
}
impl Jwm {
    /// Only process_ipc supplies owner after checking its live socket. JSON
    /// and generic command batches cannot supply that connection identity.
    pub(crate) fn remote_resize_command(
        &mut self,
        backend: &mut dyn Backend,
        owner: u64,
        name: &str,
        args: &Value,
    ) -> IpcResponse {
        match self.remote_resize_request(backend, owner, name, args) {
            Ok(value) => IpcResponse::ok(Some(value)),
            Err(error) => IpcResponse::err(error),
        }
    }
    fn remote_resize_target(
        &self,
        backend: &mut dyn Backend,
        key: ClientKey,
    ) -> Result<(i32, i32, i32, i32), String> {
        if !self.running.load(std::sync::atomic::Ordering::SeqCst)
            || !matches!(self.runtime_backend.as_str(), "x11rb" | "xcb")
        {
            return Err("remote resize requires a running X11 JWM backend".into());
        }
        if self.features.system_ui.is_locked()
            || !self.features.monitor_lock.is_empty()
            || backend.compositor_session_locked()
            || self.drag_ctl.is_some()
        {
            return Err("remote resize is blocked by lock or local interaction".into());
        }
        let c = self
            .state
            .clients
            .get(key)
            .ok_or("window instance expired")?;
        if !c.state.is_floating
            || c.state.is_fixed
            || c.state.is_fullscreen
            || c.state.is_maximized_vert
            || c.state.is_maximized_horz
            || c.state.is_pip
            || c.state.is_hidden
            || c.state.is_swallowed
            || c.state.is_dock
            || c.state.is_desktop
            || !self.is_client_visible_by_key(key)
        {
            return Err("remote resize requires a visible normal floating window".into());
        }
        if backend.property_ops().transient_for(c.win).is_some() {
            return Err("remote resize does not grant transient windows".into());
        }
        let attrs = backend
            .window_ops()
            .get_window_attributes(c.win)
            .map_err(|_| "cannot verify window mapping")?;
        if attrs.override_redirect || !attrs.map_state_viewable {
            return Err("window is not a mapped managed instance".into());
        }
        let actual = backend
            .window_ops()
            .get_geometry(c.win)
            .map_err(|_| "cannot verify native geometry")?;
        if (actual.w, actual.h) != (c.geometry.w as u32, c.geometry.h as u32) {
            return Err("native geometry is not synchronized with JWM".into());
        }
        if !(1..=65535).contains(&c.geometry.w) || !(1..=65535).contains(&c.geometry.h) {
            return Err("window dimensions exceed resize protocol limit".into());
        }
        Ok((c.geometry.x, c.geometry.y, c.geometry.w, c.geometry.h))
    }
    fn remote_resize_request(
        &mut self,
        backend: &mut dyn Backend,
        owner: u64,
        name: &str,
        args: &Value,
    ) -> Result<Value, String> {
        match name {
            "remote_resize_grant" => {
                let args: GrantArgs = serde_json::from_value(args.clone())
                    .map_err(|_| "invalid resize grant arguments")?;
                if args.window == 0 || self.remote_resize_grants.owners.contains_key(&owner) {
                    return Err("invalid window or connection already has a resize grant".into());
                }
                if !matches!(self.runtime_backend.as_str(), "x11rb" | "xcb") {
                    return Err("remote resize requires X11".into());
                }
                let win = backend
                    .resolve_window_handoff_identity(WindowHandoffIdentity::X11(args.window))
                    .ok_or("unknown X11 window")?;
                let key = self.wintoclient(win).ok_or("window is not managed")?;
                if backend
                    .property_ops()
                    .remote_resize_challenge(win)
                    .map_err(|_| "cannot verify X server challenge")?
                    != Some(args.challenge)
                {
                    return Err("X server window challenge mismatch".into());
                }
                let (_, _, width, height) = self.remote_resize_target(backend, key)?;
                if self
                    .remote_resize_grants
                    .owners
                    .values()
                    .any(|grant| grant.key == key)
                {
                    return Err("window already has a resize grant".into());
                }
                if self.remote_resize_grants.owners.len() >= 128 {
                    return Err("resize grant limit reached".into());
                }
                let id = self
                    .remote_resize_grants
                    .next
                    .checked_add(1)
                    .ok_or("resize grant sequence exhausted")?;
                self.remote_resize_grants.next = id;
                self.remote_resize_grants.owners.insert(
                    owner,
                    Grant {
                        id,
                        key,
                        challenge: args.challenge,
                    },
                );
                Ok(json!({"grant":id,"width":width,"height":height}))
            }
            "remote_resize_revoke" => {
                let args: RevokeArgs = serde_json::from_value(args.clone())
                    .map_err(|_| "invalid resize revoke arguments")?;
                if !self
                    .remote_resize_grants
                    .owners
                    .get(&owner)
                    .is_some_and(|grant| grant.id == args.grant)
                {
                    return Err("no matching connection resize grant".into());
                }
                self.remote_resize_grants.owners.remove(&owner);
                Ok(json!({"revoked":true}))
            }
            "remote_resize_apply" => {
                let args: ApplyArgs =
                    serde_json::from_value(args.clone()).map_err(|_| "invalid resize arguments")?;
                let grant = *self
                    .remote_resize_grants
                    .owners
                    .get(&owner)
                    .filter(|grant| grant.id == args.grant)
                    .ok_or("no matching connection resize grant")?;
                // Failures after matching the owner terminate the lease; no
                // retries after uncertain native results under an old grant.
                self.remote_resize_grants.owners.remove(&owner);
                let (mut x, mut y, old_w, old_h) = self.remote_resize_target(backend, grant.key)?;
                let (old_x, old_y) = (x, y);
                if args.width == 0
                    || args.height == 0
                    || args.expected_width == 0
                    || args.expected_height == 0
                    || (old_w, old_h)
                        != (
                            i32::from(args.expected_width),
                            i32::from(args.expected_height),
                        )
                {
                    return Err("invalid dimensions or stale window size".into());
                }
                let (mut w, mut h) = (i32::from(args.width), i32::from(args.height));
                self.applysizehints(backend, grant.key, &mut x, &mut y, &mut w, &mut h, false)
                    .map_err(|_| "window size hints failed")?;
                if !(1..=16384).contains(&w)
                    || !(1..=16384).contains(&h)
                    || i64::from(w) * i64::from(h) > 64 * 1024 * 1024
                {
                    return Err("constrained window dimensions exceed capture limit".into());
                }
                if (x, y) != (old_x, old_y) {
                    return Err("resize would move window; denied".into());
                }
                // Refreshing hints can reveal a fixed-size client.
                self.remote_resize_target(backend, grant.key)?;
                let win = self
                    .state
                    .clients
                    .get(grant.key)
                    .ok_or("window instance expired")?
                    .win;
                if backend
                    .property_ops()
                    .remote_resize_challenge(win)
                    .map_err(|_| "cannot verify retained challenge")?
                    != Some(grant.challenge)
                {
                    return Err("resize instance challenge expired".into());
                }
                let changed = (w, h) != (old_w, old_h);
                if changed {
                    let c = self
                        .state
                        .clients
                        .get(grant.key)
                        .ok_or("window instance expired")?;
                    let border = if backend.has_compositor() {
                        0
                    } else {
                        c.geometry.border_w.max(0) as u32
                    };
                    backend
                        .window_ops()
                        .configure_remote_resize(
                            c.win,
                            grant.challenge,
                            Geometry {
                                x: old_x,
                                y: old_y,
                                w: old_w as u32,
                                h: old_h as u32,
                                border,
                            },
                            Geometry {
                                x,
                                y,
                                w: w as u32,
                                h: h as u32,
                                border,
                            },
                        )
                        .map_err(|_| "native window configuration failed; grant revoked")?;
                    // Commit WM state only after the checked atomic native
                    // submission. Do not use resize_client, which hides errors.
                    self.animations.remove(grant.key);
                    if let Some(c) = self.state.clients.get_mut(grant.key) {
                        c.geometry.old_x = c.geometry.x;
                        c.geometry.old_y = c.geometry.y;
                        c.geometry.old_w = c.geometry.w;
                        c.geometry.old_h = c.geometry.h;
                        c.geometry.x = x;
                        c.geometry.y = y;
                        c.geometry.w = w;
                        c.geometry.h = h;
                        c.geometry.floating_x = x;
                        c.geometry.floating_y = y;
                        c.geometry.floating_w = w;
                        c.geometry.floating_h = h;
                    }
                    self.remember_scratchpad_size(grant.key);
                    self.broadcast_window_state_ipc(backend, grant.key);
                }
                self.remote_resize_grants.owners.insert(owner, grant);
                // The host must still confirm actual capture/presentation.
                Ok(
                    json!({"status":if changed {"configured"} else {"unchanged"},"width":w,"height":h,"constrained":(w,h)!=(i32::from(args.width),i32::from(args.height))}),
                )
            }
            _ => Err("unknown resize command".into()),
        }
    }
}
