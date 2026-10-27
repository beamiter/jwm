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
use zbus::{
    Connection, interface, zvariant::OwnedObjectPath, zvariant::OwnedValue, zvariant::Value,
};

use crate::capture::{self, CaptureHandle, CaptureTransport};
use crate::picker::{SourceSelection, pick_sources, selection_fits_request};
use crate::pipewire_remote;
use crate::pipewire_stream::{self, Source, StreamHandle, StreamSpec};
use crate::restore;
use crate::session::{Phase, Runtime, Session, SessionToken, StartRequest};
use crate::wayland::{OutputInfo, ToplevelInfo};

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
    token: SessionToken,
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
        // can rely on PW nodes already being gone. Dropping joins those
        // threads, so it runs on the blocking pool.
        let session = self.rt.remove_session(&self.handle, &self.token).await;
        if session.is_none() {
            return Ok(());
        }
        off_runtime(move || drop(session)).await;
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
        (SourceType::Monitor | SourceType::Window).bits()
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
        let Some(token) = self.rt.insert_session(handle_str.clone()).await else {
            return (2, HashMap::new());
        };
        let sess = SessionImpl {
            rt: self.rt.clone(),
            handle: handle_str.clone(),
            token: token.clone(),
        };
        match server.at(session_handle.clone(), sess).await {
            Ok(true) => (0, HashMap::new()),
            result => {
                warn!("CreateSession: failed to serve Session at {handle_str}: {result:?}");
                let session = self.rt.remove_session(&handle_str, &token).await;
                off_runtime(move || drop(session)).await;
                (2, HashMap::new())
            }
        }
    }

    async fn select_sources(
        &self,
        _handle: OwnedObjectPath,
        session_handle: OwnedObjectPath,
        _app_id: String,
        options: HashMap<String, OwnedValue>,
    ) -> (u32, HashMap<String, OwnedValue>) {
        info!(
            "SelectSources {session_handle} options={:?}",
            options.keys().collect::<Vec<_>>()
        );
        let handle_str = session_handle.as_str();
        let Some(token) = self
            .rt
            .with_session(handle_str, Session::begin_selection)
            .await
            .flatten()
        else {
            return (2, HashMap::new());
        };

        let parsed = parse_select_sources_opts(&options);
        let types_mask = parsed.types_mask;
        let multiple = parsed.multiple;
        let persist_mode = parsed.persist_mode;
        let paint_cursors = parsed.paint_cursors;
        let restore_token_in = parsed.restore_token;
        let want_monitor = types_mask & (SourceType::Monitor as u32) != 0;
        let want_window = types_mask & (SourceType::Window as u32) != 0;

        let (outputs, toplevels) = {
            let snap = self.rt.wayland().lock().expect("wayland snapshot mutex");
            (snap.outputs.clone(), snap.toplevels.clone())
        };
        let request = SourceRequest {
            outputs,
            toplevels,
            restore_token: restore_token_in,
            want_monitor,
            want_window,
            multiple,
        };
        let Some((selection, honored_token)) =
            off_runtime(move || choose_sources(request)).await.flatten()
        else {
            self.rt
                .with_current_session(handle_str, &token, |s| s.phase = Phase::Failed)
                .await;
            info!("SelectSources cancelled by user or invalid source override");
            return (1, HashMap::new());
        };
        info!(
            "SelectSources picked {} output(s), {} toplevel(s)",
            selection.outputs.len(),
            selection.toplevels.len()
        );

        let configured = self
            .rt
            .with_current_session(handle_str, &token, |s| {
                s.selection = Some(selection);
                s.persist_mode = persist_mode;
                s.restore_token = honored_token;
                s.paint_cursors = paint_cursors;
                s.phase = Phase::Selected;
            })
            .await;
        (if configured.is_some() { 0 } else { 1 }, HashMap::new())
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

        let handle_str = session_handle.as_str();
        let Some(StartRequest {
            token,
            selection,
            persist_mode,
            prior_token,
            paint_cursors,
        }) = self
            .rt
            .with_session(handle_str, Session::begin_start)
            .await
            .flatten()
        else {
            return (2, HashMap::new());
        };

        // Each capture and PipeWire stream negotiates for up to 5 seconds.
        let sources = selection.clone();
        let cancellation = token.clone();
        let Some(StartedStreams {
            streams_meta,
            handles,
            captures,
        }) = off_runtime(move || start_streams(&sources, paint_cursors, &cancellation)).await
        else {
            self.rt
                .with_current_session(handle_str, &token, |s| s.phase = Phase::Failed)
                .await;
            return (2, HashMap::new());
        };

        if streams_meta.is_empty() {
            warn!("Start {session_handle}: no streams could be started");
            self.rt
                .with_current_session(handle_str, &token, |s| s.phase = Phase::Failed)
                .await;
            off_runtime(move || drop((handles, captures))).await;
            return (if token.is_cancelled() { 1 } else { 2 }, HashMap::new());
        }

        // Only this session identity can publish the result of its one Start.
        // A close/recreate while negotiation runs cannot resurrect old streams.
        let mut started = Some((handles, captures));
        let stored = self
            .rt
            .with_current_session(handle_str, &token, |s| {
                let (handles, captures) = started.take().expect("streams are stored once");
                s.streams = handles;
                s.captures = captures;
                s.phase = Phase::Started;
            })
            .await;
        off_runtime(move || drop(started)).await;
        if stored.is_none() {
            return (1, HashMap::new());
        }

        let mut results = HashMap::new();
        if let Ok(v) = Value::from(streams_meta).try_into() {
            results.insert("streams".to_string(), v);
        }
        if persist_mode > 0 && (!selection.outputs.is_empty() || !selection.toplevels.is_empty()) {
            // The restore store writes its file synchronously.
            let token = off_runtime(move || match prior_token {
                Some(t) => {
                    restore::touch(&t, &selection, persist_mode);
                    t
                }
                None => restore::save_new(&selection, persist_mode),
            })
            .await;
            if let Some(token) = token {
                if let Ok(v) = restore::encode_data(&token) {
                    results.insert("restore_data".to_string(), v);
                }
                if let Ok(v) = Value::from(persist_mode).try_into() {
                    results.insert("persist_mode".to_string(), v);
                }
            }
        }
        if token.is_cancelled() {
            return (1, HashMap::new());
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
        let handle_str = session_handle.as_str();
        let token = self
            .rt
            .with_session(handle_str, |s| s.started_token())
            .await
            .flatten()
            .ok_or_else(|| zbus::fdo::Error::Failed("session has not been started".into()))?;
        // Hand the caller a fresh UNIX-socket fd connected to the user
        // PipeWire daemon. They feed it to `pw_context_connect_fd()` on
        // their side to skip the usual env-var-based discovery.
        let owned = pipewire_remote::connect()
            .await
            .map_err(|e| zbus::fdo::Error::Failed(format!("OpenPipeWireRemote: {e}")))?;
        if self
            .rt
            .with_current_session(handle_str, &token, |s| s.started_token())
            .await
            .flatten()
            .is_none()
        {
            return Err(zbus::fdo::Error::Failed(
                "session closed while connecting".into(),
            ));
        }
        Ok(zbus::zvariant::OwnedFd::from(owned))
    }
}

/// Run work that blocks on tokio's blocking pool: the consent picker waiting
/// for the user, capture and PipeWire negotiation, restore-file writes and
/// joining stream threads. The runtime has two async workers. Blocking one
/// directly halves D-Bus throughput, and two open pickers stall every call
/// of every session, `Session.Close` included. `None` means the work
/// panicked.
async fn off_runtime<T, F>(work: F) -> Option<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    match tokio::task::spawn_blocking(work).await {
        Ok(value) => Some(value),
        Err(error) => {
            warn!("blocking portal work failed: {error}");
            None
        }
    }
}

/// What `SelectSources` asked for, resolved against one snapshot.
struct SourceRequest {
    outputs: Vec<OutputInfo>,
    toplevels: Vec<ToplevelInfo>,
    restore_token: Option<String>,
    want_monitor: bool,
    want_window: bool,
    multiple: bool,
}

/// The blocking half of `SelectSources`: JWM IPC for connector names, the
/// restore store, then the picker. Returns the selection and the restore
/// token it honored, or `None` when the user cancelled.
fn choose_sources(request: SourceRequest) -> Option<(SourceSelection, Option<String>)> {
    let SourceRequest {
        mut outputs,
        toplevels,
        restore_token,
        want_monitor,
        want_window,
        multiple,
    } = request;
    crate::ipc::enrich_outputs_with_connectors(&mut outputs);

    // Try the restore path first — if the caller handed us a token and the
    // stored sources still resolve, skip the picker entirely.
    if let Some(tok) = restore_token.as_deref()
        && let Some((sel, _prev_mode)) = restore::resolve(tok, &outputs, &toplevels)
    {
        let filtered = SourceSelection {
            outputs: if want_monitor {
                sel.outputs
            } else {
                Vec::new()
            },
            toplevels: if want_window {
                sel.toplevels
            } else {
                Vec::new()
            },
        };
        if selection_fits_request(&filtered, multiple) {
            info!("SelectSources honoring saved source selection");
            return Some((filtered, Some(tok.to_string())));
        }
    }

    let permitted_outputs = if want_monitor {
        outputs.as_slice()
    } else {
        &[]
    };
    let permitted_toplevels = if want_window {
        toplevels.as_slice()
    } else {
        &[]
    };
    pick_sources(permitted_outputs, permitted_toplevels, multiple)
        .map(|selection| (selection, None))
}

/// Streams `Start` brought up, in the order of its `streams` result.
struct StartedStreams {
    streams_meta: Vec<(u32, HashMap<String, OwnedValue>)>,
    handles: Vec<StreamHandle>,
    captures: Vec<CaptureHandle>,
}

/// The blocking half of `Start`: one capture thread and one PipeWire stream
/// per selected source, each negotiating before it returns.
fn start_streams(
    selection: &SourceSelection,
    paint_cursors: bool,
    cancellation: &SessionToken,
) -> StartedStreams {
    // For each picked output, spawn:
    //   1. Wayland capture thread (negotiates real buffer_size + format,
    //      then pumps frames into a SharedFrame).
    //   2. PipeWire producer thread (drains the SharedFrame in on_process).
    //
    let mut streams_meta: Vec<(u32, HashMap<String, OwnedValue>)> = Vec::new();
    let mut handles: Vec<StreamHandle> = Vec::new();
    let mut captures: Vec<CaptureHandle> = Vec::new();
    for (idx, o) in selection.outputs.iter().enumerate() {
        if cancellation.is_cancelled() {
            break;
        }
        let capture = match capture::spawn_output_capture(o.name.clone(), paint_cursors) {
            Ok(c) => c,
            Err(e) => {
                warn!(
                    "Start: failed to spawn capture for output `{}`: {e}",
                    o.name
                );
                continue;
            }
        };
        if cancellation.is_cancelled() {
            break;
        }
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
        if cancellation.is_cancelled() {
            break;
        }
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
        if cancellation.is_cancelled() {
            break;
        }
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
    StartedStreams {
        streams_meta,
        handles,
        captures,
    }
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

/// Parsed view of the `options` dict from `SelectSources`. Extracted so the
/// option-parsing rules (defaults, bitmask checks, type coercion) can be unit
/// tested without a live D-Bus stack or the wayland snapshot.
#[derive(Debug, PartialEq)]
pub(crate) struct SelectSourcesOpts {
    pub types_mask: u32,
    pub multiple: bool,
    pub persist_mode: u32,
    pub cursor_mode: u32,
    /// True iff `cursor_mode` has the Embedded bit set. The compositor's
    /// `paint_cursors` flag mirrors this — Hidden (1) → false, Embedded (2)
    /// → true, Metadata (4) → false (we don't render the cursor; we'd
    /// instead emit metadata, which we don't advertise yet).
    pub paint_cursors: bool,
    pub restore_token: Option<String>,
}

pub(crate) fn parse_select_sources_opts(
    options: &HashMap<String, OwnedValue>,
) -> SelectSourcesOpts {
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
    let cursor_mode = options
        .get("cursor_mode")
        .and_then(|v| u32::try_from(v).ok())
        .unwrap_or(CursorMode::Hidden as u32);
    let paint_cursors = cursor_mode & (CursorMode::Embedded as u32) != 0;
    let restore_token = options.get("restore_data").and_then(restore::decode_data);
    SelectSourcesOpts {
        types_mask,
        multiple,
        persist_mode,
        cursor_mode,
        paint_cursors,
        restore_token,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::Value;

    fn ov_u32(n: u32) -> OwnedValue {
        Value::from(n).try_into().unwrap()
    }
    fn ov_bool(b: bool) -> OwnedValue {
        Value::from(b).try_into().unwrap()
    }
    fn ov_str(s: &str) -> OwnedValue {
        Value::from(s).try_into().unwrap()
    }

    #[tokio::test]
    async fn session_lifecycle_is_enforced_over_a_private_bus() {
        use std::io::{BufRead, BufReader};
        use std::process::{Child, Command, Stdio};
        use std::sync::{Arc, Mutex};

        struct Bus(Child);
        impl Drop for Bus {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let mut bus = Bus(Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("install dbus-daemon to run portal integration tests"));
        let mut address = String::new();
        BufReader::new(bus.0.stdout.take().unwrap())
            .read_line(&mut address)
            .unwrap();
        let rt = Runtime::start(Arc::new(Mutex::new(Default::default())))
            .await
            .unwrap();
        let service = zbus::connection::Builder::address(address.trim())
            .unwrap()
            .name("org.freedesktop.impl.portal.desktop.jwm")
            .unwrap()
            .serve_at(
                "/org/freedesktop/portal/desktop",
                ScreenCast { rt: rt.clone() },
            )
            .unwrap()
            .build()
            .await
            .unwrap();
        let client = zbus::connection::Builder::address(address.trim())
            .unwrap()
            .build()
            .await
            .unwrap();
        let proxy = zbus::Proxy::new(
            &client,
            "org.freedesktop.impl.portal.desktop.jwm",
            "/org/freedesktop/portal/desktop",
            "org.freedesktop.impl.portal.ScreenCast",
        )
        .await
        .unwrap();
        let request = OwnedObjectPath::try_from("/request/test").unwrap();
        let session = OwnedObjectPath::try_from("/session/test").unwrap();
        let options = || HashMap::<String, OwnedValue>::new();
        let create = || (request.clone(), session.clone(), "test.app", options());
        let (code, _): (u32, HashMap<String, OwnedValue>) =
            proxy.call("CreateSession", &create()).await.unwrap();
        assert_eq!(code, 0);
        rt.with_session(session.as_str(), |s| s.persist_mode = 2)
            .await;
        let (code, _): (u32, HashMap<String, OwnedValue>) =
            proxy.call("CreateSession", &create()).await.unwrap();
        assert_eq!(code, 2);
        assert_eq!(
            rt.with_session(session.as_str(), |s| s.persist_mode).await,
            Some(2)
        );

        let unknown = OwnedObjectPath::try_from("/session/unknown").unwrap();
        let (code, _): (u32, HashMap<String, OwnedValue>) = proxy
            .call(
                "SelectSources",
                &(request.clone(), unknown.clone(), "test.app", options()),
            )
            .await
            .unwrap();
        assert_eq!(code, 2);
        let (code, _): (u32, HashMap<String, OwnedValue>) = proxy
            .call(
                "Start",
                &(request.clone(), session.clone(), "test.app", "", options()),
            )
            .await
            .unwrap();
        assert_eq!(code, 2);
        let remote: zbus::Result<zbus::zvariant::OwnedFd> = proxy
            .call("OpenPipeWireRemote", &(session.clone(), options()))
            .await;
        assert!(
            remote.is_err(),
            "an unstarted session must not receive a PipeWire FD"
        );

        // A pre-existing D-Bus object must make creation fail and roll back
        // the reserved map entry, rather than return an unusable session.
        let blocked = OwnedObjectPath::try_from("/session/blocked").unwrap();
        service
            .object_server()
            .at(
                blocked.clone(),
                SessionImpl {
                    rt: rt.clone(),
                    handle: blocked.to_string(),
                    token: SessionToken::default(),
                },
            )
            .await
            .unwrap();
        let (code, _): (u32, HashMap<String, OwnedValue>) = proxy
            .call(
                "CreateSession",
                &(request.clone(), blocked.clone(), "test.app", options()),
            )
            .await
            .unwrap();
        assert_eq!(code, 2);
        assert!(rt.with_session(blocked.as_str(), |_| ()).await.is_none());

        let token = rt
            .with_session(session.as_str(), |s| s.token_for_test())
            .await
            .unwrap();
        let session_proxy = zbus::Proxy::new(
            &client,
            "org.freedesktop.impl.portal.desktop.jwm",
            session.clone(),
            "org.freedesktop.impl.portal.Session",
        )
        .await
        .unwrap();
        session_proxy.call::<_, _, ()>("Close", &()).await.unwrap();
        assert!(token.is_cancelled());
        assert!(rt.with_session(session.as_str(), |_| ()).await.is_none());
        let (code, _): (u32, HashMap<String, OwnedValue>) =
            proxy.call("CreateSession", &create()).await.unwrap();
        assert_eq!(code, 0);
    }

    #[test]
    fn empty_options_yield_documented_defaults() {
        let p = parse_select_sources_opts(&HashMap::new());
        assert_eq!(p.types_mask, SourceType::Monitor as u32);
        assert!(!p.multiple);
        assert_eq!(p.persist_mode, 0);
        assert_eq!(p.cursor_mode, CursorMode::Hidden as u32);
        assert!(
            !p.paint_cursors,
            "the backend specification defaults to Hidden"
        );
        assert!(p.restore_token.is_none());
    }

    #[test]
    fn cursor_mode_hidden_disables_paint_cursors() {
        let mut opts = HashMap::new();
        opts.insert("cursor_mode".into(), ov_u32(CursorMode::Hidden as u32));
        let p = parse_select_sources_opts(&opts);
        assert!(!p.paint_cursors);
        assert_eq!(p.cursor_mode, CursorMode::Hidden as u32);
    }

    #[test]
    fn cursor_mode_metadata_does_not_imply_painted_cursors() {
        // Metadata mode would emit cursor info in PW metadata; we don't paint.
        // Defensive: an unknown bit shouldn't accidentally enable painting.
        let mut opts = HashMap::new();
        opts.insert("cursor_mode".into(), ov_u32(CursorMode::Metadata as u32));
        let p = parse_select_sources_opts(&opts);
        assert!(!p.paint_cursors);
    }

    #[test]
    fn embedded_combined_with_metadata_still_paints() {
        let mut opts = HashMap::new();
        let mixed = (CursorMode::Embedded as u32) | (CursorMode::Metadata as u32);
        opts.insert("cursor_mode".into(), ov_u32(mixed));
        let p = parse_select_sources_opts(&opts);
        assert!(
            p.paint_cursors,
            "Embedded bit set anywhere → composite cursors"
        );
    }

    #[test]
    fn types_mask_window_only() {
        let mut opts = HashMap::new();
        opts.insert("types".into(), ov_u32(SourceType::Window as u32));
        let p = parse_select_sources_opts(&opts);
        assert_eq!(p.types_mask, SourceType::Window as u32);
        let want_monitor = p.types_mask & (SourceType::Monitor as u32) != 0;
        let want_window = p.types_mask & (SourceType::Window as u32) != 0;
        assert!(!want_monitor);
        assert!(want_window);
    }

    #[test]
    fn types_mask_monitor_and_window() {
        let mut opts = HashMap::new();
        let both = (SourceType::Monitor as u32) | (SourceType::Window as u32);
        opts.insert("types".into(), ov_u32(both));
        let p = parse_select_sources_opts(&opts);
        let want_monitor = p.types_mask & (SourceType::Monitor as u32) != 0;
        let want_window = p.types_mask & (SourceType::Window as u32) != 0;
        assert!(want_monitor);
        assert!(want_window);
    }

    #[test]
    fn multiple_and_persist_mode_round_trip() {
        let mut opts = HashMap::new();
        opts.insert("multiple".into(), ov_bool(true));
        opts.insert("persist_mode".into(), ov_u32(2));
        let p = parse_select_sources_opts(&opts);
        assert!(p.multiple);
        assert_eq!(p.persist_mode, 2);
    }

    #[test]
    fn backend_restore_data_extracts_private_token() {
        let mut opts = HashMap::new();
        opts.insert(
            "restore_data".into(),
            restore::encode_data("abc123deadbeef").unwrap(),
        );
        let p = parse_select_sources_opts(&opts);
        assert_eq!(p.restore_token.as_deref(), Some("abc123deadbeef"));
    }

    #[test]
    fn frontend_restore_token_is_not_a_backend_restore_grant() {
        let mut opts = HashMap::new();
        opts.insert("restore_token".into(), ov_str("abc123deadbeef"));
        assert!(parse_select_sources_opts(&opts).restore_token.is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn blocking_work_leaves_the_runtime_free_for_other_calls() {
        // A picker that waits until another task on this one-thread runtime
        // has run: run inline, it would hold the thread for the whole wait.
        let (release, released) = std::sync::mpsc::channel::<()>();
        let picker = tokio::spawn(off_runtime(move || {
            released
                .recv_timeout(std::time::Duration::from_secs(10))
                .is_ok()
        }));
        let other_call = tokio::spawn(async move { release.send(()).unwrap() });
        other_call.await.unwrap();
        assert_eq!(picker.await.unwrap(), Some(true));
    }

    #[test]
    fn handlers_keep_blocking_work_off_the_async_workers() {
        let source = include_str!("dbus.rs");
        let helper = format!("{}{}", "async fn off_", "runtime<");
        let handlers = &source[..source.find(&helper).expect("helper is defined")];
        for work in [
            "choose_sources(request)",
            "start_streams(&sources",
            "drop(session)",
            "drop(started)",
            "match prior_token",
        ] {
            let routed = format!("{}{work}", "off_runtime(move || ");
            assert!(
                handlers.contains(&routed),
                "`{work}` must run off the runtime"
            );
        }
        for direct in [
            "pick_sources(",
            "spawn_output_capture(",
            "spawn_toplevel_capture(",
            "pipewire_stream::spawn(",
            "restore::resolve(",
            "enrich_outputs_with_connectors(",
        ] {
            assert!(
                !handlers.contains(direct),
                "`{direct}` blocks an async worker"
            );
        }
    }

    #[test]
    fn wrong_type_for_option_falls_back_to_default_not_panic() {
        // Defense against misbehaving clients that pass e.g. a string where a
        // u32 is expected. Parser must silently ignore and use the default.
        let mut opts = HashMap::new();
        opts.insert("types".into(), ov_str("not a u32"));
        opts.insert("persist_mode".into(), ov_bool(true));
        let p = parse_select_sources_opts(&opts);
        assert_eq!(p.types_mask, SourceType::Monitor as u32);
        assert_eq!(p.persist_mode, 0);
    }
}
