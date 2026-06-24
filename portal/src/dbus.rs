//! zbus implementation of `org.freedesktop.impl.portal.ScreenCast`.
//!
//! This interface is the *backend* one (`impl.portal.ScreenCast`), called by
//! `xdg-desktop-portal` after it has done its own UI/permissions dance.
//! Methods get a request `handle`, options dict, and must return a tuple
//! `(response, results)` where `response = 0` means success.

use std::collections::HashMap;

use enumflags2::{BitFlags, bitflags};
use log::{info, warn};
use zbus::object_server::{ObjectServer, SignalEmitter};
use zbus::{Connection, interface, zvariant::OwnedObjectPath, zvariant::OwnedValue, zvariant::Value};

use crate::capture::{self, CaptureHandle, CaptureTransport};
use crate::picker::{SourceSelection, pick_outputs, pick_windows};
use crate::pipewire_stream::{self, Source, StreamHandle, StreamSpec};
use crate::restore;
use crate::session::Runtime;

/// Source type bits per the portal spec.
#[bitflags]
#[repr(u32)]
#[derive(Copy, Clone, Debug)]
pub enum SourceType {
    Monitor = 1,
    Window = 2,
    Virtual = 4,
}

/// Cursor mode bits per the portal spec.
#[bitflags]
#[repr(u32)]
#[derive(Copy, Clone, Debug)]
pub enum CursorMode {
    Hidden = 1,
    Embedded = 2,
    Metadata = 4,
}

struct ScreenCast {
    rt: Runtime,
}

/// Per-session backend object served at the dynamic `session_handle` path.
/// `xdg-desktop-portal` translates the client's `portal.Session.Close` into a
/// call on this object so we can tear streams down cleanly and emit `Closed`.
struct SessionImpl {
    rt: Runtime,
    handle: String,
}

#[interface(name = "org.freedesktop.impl.portal.Session")]
impl SessionImpl {
    #[zbus(property)]
    fn version(&self) -> u32 {
        1
    }

    async fn close(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<()> {
        info!("Session.Close {}", self.handle);
        // Drop streams + capture threads first so the Closed signal observers
        // can rely on PW nodes already being gone.
        let _ = self.rt.remove_session(&self.handle).await;
        let _ = Self::closed(&emitter).await;
        let path = self.handle.clone();
        let _ = server.remove::<SessionImpl, _>(path.as_str()).await;
        Ok(())
    }

    #[zbus(signal)]
    async fn closed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

#[interface(name = "org.freedesktop.impl.portal.ScreenCast")]
impl ScreenCast {
    #[zbus(property)]
    fn version(&self) -> u32 {
        4
    }

    #[zbus(property, name = "AvailableSourceTypes")]
    fn available_source_types(&self) -> u32 {
        BitFlags::from(SourceType::Monitor | SourceType::Window).bits()
    }

    #[zbus(property, name = "AvailableCursorModes")]
    fn available_cursor_modes(&self) -> u32 {
        (BitFlags::from(CursorMode::Hidden) | CursorMode::Embedded).bits()
    }

    async fn create_session(
        &self,
        _handle: OwnedObjectPath,
        session_handle: OwnedObjectPath,
        _app_id: String,
        _options: HashMap<String, OwnedValue>,
        #[zbus(object_server)] server: &ObjectServer,
    ) -> (u32, HashMap<String, OwnedValue>) {
        info!("CreateSession {session_handle}");
        let handle_str = session_handle.to_string();
        self.rt.insert_session(handle_str.clone()).await;
        let sess = SessionImpl {
            rt: self.rt.clone(),
            handle: handle_str.clone(),
        };
        if let Err(e) = server.at(session_handle.clone(), sess).await {
            warn!("CreateSession: failed to serve Session at {handle_str}: {e}");
        }
        (0, HashMap::new())
    }

    async fn select_sources(
        &self,
        _handle: OwnedObjectPath,
        session_handle: OwnedObjectPath,
        _app_id: String,
        options: HashMap<String, OwnedValue>,
    ) -> (u32, HashMap<String, OwnedValue>) {
        info!("SelectSources {session_handle} options={:?}", options.keys().collect::<Vec<_>>());

        let types_mask = options
            .get("types")
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(SourceType::Monitor as u32);
        let multiple = options
            .get("multiple")
            .and_then(|v| bool::try_from(v).ok())
            .unwrap_or(false);
        let persist_mode = options
            .get("persist_mode")
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(0);
        // cursor_mode is a bitmask but the client sets at most one bit per
        // session (Hidden=1, Embedded=2, Metadata=4). Default to Embedded —
        // matches what most apps expect when they didn't ask explicitly.
        let cursor_mode = options
            .get("cursor_mode")
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(CursorMode::Embedded as u32);
        let paint_cursors = cursor_mode & (CursorMode::Embedded as u32) != 0;
        let restore_token_in = options
            .get("restore_token")
            .and_then(|v| <&str>::try_from(&**v).ok().map(str::to_string));
        let want_monitor = types_mask & (SourceType::Monitor as u32) != 0;
        let want_window = types_mask & (SourceType::Window as u32) != 0;

        let (outputs, toplevels) = {
            let snap = self.rt.wayland().lock().expect("wayland snapshot mutex");
            (snap.outputs.clone(), snap.toplevels.clone())
        };

        // Try the restore path first — if the caller handed us a token and the
        // stored sources still resolve, skip the picker entirely.
        let mut selection: Option<SourceSelection> = None;
        let mut honored_token: Option<String> = None;
        if let Some(tok) = restore_token_in.as_deref() {
            if let Some((sel, _prev_mode)) = restore::resolve(tok, &outputs, &toplevels) {
                let filtered = SourceSelection {
                    outputs: if want_monitor { sel.outputs } else { Vec::new() },
                    toplevels: if want_window { sel.toplevels } else { Vec::new() },
                };
                if !filtered.outputs.is_empty() || !filtered.toplevels.is_empty() {
                    info!("SelectSources honoring restore_token `{tok}`");
                    selection = Some(filtered);
                    honored_token = Some(tok.to_string());
                }
            }
        }

        let selection = selection.unwrap_or_else(|| SourceSelection {
            outputs: if want_monitor {
                pick_outputs(&outputs, multiple)
            } else {
                Vec::new()
            },
            toplevels: if want_window {
                pick_windows(&toplevels, multiple)
            } else {
                Vec::new()
            },
        });
        info!(
            "SelectSources picked {} output(s), {} toplevel(s)",
            selection.outputs.len(),
            selection.toplevels.len()
        );

        let _ = self
            .rt
            .with_session(&session_handle.to_string(), |s| {
                s.selection = Some(selection);
                s.persist_mode = persist_mode;
                s.restore_token = honored_token;
                s.paint_cursors = paint_cursors;
            })
            .await;
        (0, HashMap::new())
    }

    async fn start(
        &self,
        _handle: OwnedObjectPath,
        session_handle: OwnedObjectPath,
        _app_id: String,
        _parent_window: String,
        _options: HashMap<String, OwnedValue>,
    ) -> (u32, HashMap<String, OwnedValue>) {
        info!("Start {session_handle}");

        // Pull the resolved selection out of the session — SelectSources put it
        // there. Without it, this is a no-op-but-success Start.
        let (selection, persist_mode, prior_token, paint_cursors) = self
            .rt
            .with_session(&session_handle.to_string(), |s| {
                (
                    s.selection.clone().unwrap_or_default(),
                    s.persist_mode,
                    s.restore_token.clone(),
                    s.paint_cursors,
                )
            })
            .await
            .unwrap_or((SourceSelection::default(), 0, None, true));

        // For each picked output, spawn:
        //   1. Wayland capture thread (negotiates real buffer_size + format,
        //      then pumps frames into a SharedFrame).
        //   2. PipeWire producer thread (drains the SharedFrame in on_process).
        //
        // Toplevels still use the zero-fill placeholder — the wl_output side
        // needed for re-binding inside a fresh capture thread isn't relevant
        // for toplevels, which would need their own re-discovery path.
        let mut streams_meta: Vec<(u32, HashMap<String, OwnedValue>)> = Vec::new();
        let mut handles: Vec<StreamHandle> = Vec::new();
        let mut captures: Vec<CaptureHandle> = Vec::new();
        for (idx, o) in selection.outputs.iter().enumerate() {
            let capture = match capture::spawn_output_capture(o.name.clone(), paint_cursors) {
                Ok(c) => c,
                Err(e) => {
                    warn!("Start: failed to spawn capture for output `{}`: {e}", o.name);
                    continue;
                }
            };
            let spec = StreamSpec {
                width: capture.width,
                height: capture.height,
                framerate_num: capture.framerate_num,
                framerate_den: capture.framerate_den,
            };
            let name = if o.name.is_empty() {
                format!("jwm-output-{idx}")
            } else {
                format!("jwm-output-{}", o.name)
            };
            let source = match &capture.transport {
                CaptureTransport::Shm(f) => Source::Shm(f.clone()),
                CaptureTransport::Dmabuf(b) => Source::Dmabuf(b.clone()),
            };
            match pipewire_stream::spawn(spec, name, source) {
                Ok(h) => {
                    let mut props: HashMap<String, OwnedValue> = HashMap::new();
                    if let Ok(v) = Value::from((spec.width as i32, spec.height as i32)).try_into() {
                        props.insert("size".to_string(), v);
                    }
                    if let Ok(v) = Value::from(SourceType::Monitor as u32).try_into() {
                        props.insert("source_type".to_string(), v);
                    }
                    streams_meta.push((h.node_id, props));
                    handles.push(h);
                    captures.push(capture);
                }
                Err(e) => warn!("Start: failed to spawn output PW stream: {e}"),
            }
        }
        for (idx, t) in selection.toplevels.iter().enumerate() {
            let capture = match capture::spawn_toplevel_capture(t.identifier.clone(), paint_cursors) {
                Ok(c) => c,
                Err(e) => {
                    warn!(
                        "Start: failed to spawn capture for toplevel `{}` (app_id=`{}`): {e}",
                        t.identifier, t.app_id
                    );
                    continue;
                }
            };
            let spec = StreamSpec {
                width: capture.width,
                height: capture.height,
                framerate_num: capture.framerate_num,
                framerate_den: capture.framerate_den,
            };
            let name = if t.app_id.is_empty() {
                format!("jwm-window-{idx}")
            } else {
                format!("jwm-window-{}", t.app_id)
            };
            let source = match &capture.transport {
                CaptureTransport::Shm(f) => Source::Shm(f.clone()),
                CaptureTransport::Dmabuf(b) => Source::Dmabuf(b.clone()),
            };
            match pipewire_stream::spawn(spec, name, source) {
                Ok(h) => {
                    let mut props: HashMap<String, OwnedValue> = HashMap::new();
                    if let Ok(v) = Value::from((spec.width as i32, spec.height as i32)).try_into() {
                        props.insert("size".to_string(), v);
                    }
                    if let Ok(v) = Value::from(SourceType::Window as u32).try_into() {
                        props.insert("source_type".to_string(), v);
                    }
                    streams_meta.push((h.node_id, props));
                    handles.push(h);
                    captures.push(capture);
                }
                Err(e) => warn!("Start: failed to spawn window PW stream: {e}"),
            }
        }

        let _ = self
            .rt
            .with_session(&session_handle.to_string(), |s| {
                s.streams = handles;
                s.captures = captures;
            })
            .await;

        let mut results = HashMap::new();
        if let Ok(v) = Value::from(streams_meta).try_into() {
            results.insert("streams".to_string(), v);
        }
        if persist_mode > 0 && (!selection.outputs.is_empty() || !selection.toplevels.is_empty()) {
            let token = match prior_token {
                Some(t) => {
                    restore::touch(&t, &selection, persist_mode);
                    t
                }
                None => restore::save_new(&selection, persist_mode),
            };
            if let Ok(v) = Value::from(token).try_into() {
                results.insert("restore_token".to_string(), v);
            }
            if let Ok(v) = Value::from(persist_mode).try_into() {
                results.insert("persist_mode".to_string(), v);
            }
        }
        info!("Start {session_handle} returning streams");
        (0, results)
    }

    async fn open_pipe_wire_remote(
        &self,
        session_handle: OwnedObjectPath,
        _options: HashMap<String, OwnedValue>,
    ) -> zbus::fdo::Result<zbus::zvariant::OwnedFd> {
        info!("OpenPipeWireRemote {session_handle}");
        // Hand the caller a fresh UNIX-socket fd connected to the user
        // PipeWire daemon. They feed it to `pw_context_connect_fd()` on
        // their side to skip the usual env-var-based discovery.
        let path = pipewire_socket_path();
        let stream = std::os::unix::net::UnixStream::connect(&path).map_err(|e| {
            zbus::fdo::Error::Failed(format!(
                "OpenPipeWireRemote: connect to PipeWire socket {path:?}: {e}"
            ))
        })?;
        let owned: std::os::fd::OwnedFd = stream.into();
        Ok(zbus::zvariant::OwnedFd::from(owned))
    }
}

/// Resolve the PipeWire daemon socket path the client should connect to.
/// Honors `PIPEWIRE_REMOTE` if set, else `$XDG_RUNTIME_DIR/pipewire-0` (the
/// standard location written by PipeWire's own systemd unit).
fn pipewire_socket_path() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("PIPEWIRE_REMOTE") {
        return std::path::PathBuf::from(p);
    }
    let runtime = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    let mut p = std::path::PathBuf::from(runtime);
    p.push("pipewire-0");
    p
}

pub async fn serve(rt: Runtime) -> Result<Connection, Box<dyn std::error::Error + Send + Sync>> {
    let conn = zbus::connection::Builder::session()?
        .name("org.freedesktop.impl.portal.desktop.jwm")?
        .serve_at("/org/freedesktop/portal/desktop", ScreenCast { rt })?
        .build()
        .await?;
    info!("D-Bus service registered as org.freedesktop.impl.portal.desktop.jwm");
    Ok(conn)
}
