//! Read the X11 desktop through the Composite overlay or a staged root snapshot.
//!
//! This is intentionally kept in the out-of-process LAN MVP.  A slow encoder
//! or peer can therefore never stall JWM's display event loop.  Both the
//! x11rb and xcb JWM backends share one X server, so one small X11 client covers
//! both transports. Root downscaling snapshots children into a pixmap before
//! XRender; the root Window itself is never used as a Picture.

use super::{RemoteError, RemoteResult};
use image::{RgbImage, RgbaImage, imageops::FilterType};
use std::borrow::Cow;
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::ptr::{self, NonNull};
use std::time::{Duration, Instant};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::cookie::Cookie;
use x11rb::errors::{ConnectionError, ReplyError, ReplyOrIdError};
use x11rb::image::{BitsPerPixel, Image as XImage, ImageOrder, PixelLayout, ScanlinePad};
use x11rb::protocol::render::{CreatePictureAux, PictOp, Pictformat, Picture, Repeat, Transform};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ChangeWindowAttributesAux, ConnectionExt as _, CreateGCAux, CreateWindowAux,
    EventMask, Format, Gcontext, ImageFormat, Pixmap, PropMode, QueryPointerReply, Screen,
    SubwindowMode, VisualClass, Visualid, Visualtype, Window, WindowClass,
};
use x11rb::protocol::{Event, composite, damage, randr, render, shm, xfixes};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::x11_utils::TryParseFd;

const COMPOSITE_CLIENT_VERSION: (u32, u32) = (0, 4);
const COMPOSITE_OVERLAY_VERSION: (u32, u32) = (0, 3);
const XFIXES_CLIENT_VERSION: (u32, u32) = (5, 0);
const RANDR_CLIENT_VERSION: (u32, u32) = (1, 6);
const RENDER_CLIENT_VERSION: (u32, u32) = (0, 11);
const RENDER_TRANSFORM_VERSION: (u32, u32) = (0, 10);
const DAMAGE_CLIENT_VERSION: (u32, u32) = (1, 1);
const SHM_FD_VERSION: (u16, u16) = (1, 2);
const REMOTE_CAPTURE_OWNER: &[u8] = b"_JWM_REMOTE_CAPTURE_OWNER";
const OVERLAY_RETRY_DELAY: Duration = Duration::from_secs(1);
pub(crate) const DAMAGE_FORCE_REFRESH: Duration = Duration::from_secs(2);
const ROOT_STAGING_MAX_BYTES: usize = 64 * 1024 * 1024;

/// Which X drawable supplies the remote desktop image.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CaptureSource {
    /// Prefer the compositor overlay and fall back to the root drawable when
    /// Composite is unavailable.
    #[default]
    Auto,
    /// Capture the Composite overlay.  This includes JWM's effects and system
    /// UI, but requires the Composite extension.
    Overlay,
    /// Capture the root drawable.  This is a compatibility fallback for X
    /// servers whose overlay cannot be read back.
    Root,
}

impl std::str::FromStr for CaptureSource {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "overlay" | "compositor" => Ok(Self::Overlay),
            "root" => Ok(Self::Root),
            _ => Err(format!(
                "unknown capture source {value:?}; expected auto, overlay, or root"
            )),
        }
    }
}

/// One top-to-bottom RGB frame plus the unscaled root coordinate space.
#[derive(Debug)]
pub struct CapturedFrame {
    pub image: RgbImage,
    pub source_width: u16,
    pub source_height: u16,
}

/// Result of one scheduled capture attempt.
#[derive(Debug)]
pub enum CaptureOutcome {
    /// A fresh drawable readback, ready for the host mailbox.
    Frame(CapturedFrame),
    /// The overlay, geometry and software-composited cursor are unchanged.
    NoChange,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CaptureFailureKind {
    Render,
    Readback,
    Fatal,
}

struct CaptureFailure {
    kind: CaptureFailureKind,
    error: RemoteError,
}

enum CaptureDrawableOutcome {
    Frame(CapturedDrawable),
    RootGeometryChanged,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RootSnapshotRaceAction {
    Retry,
    CpuFallback,
}

fn root_snapshot_race_action(
    retry_available: bool,
    source_changed: bool,
) -> RootSnapshotRaceAction {
    if retry_available || source_changed {
        RootSnapshotRaceAction::Retry
    } else {
        RootSnapshotRaceAction::CpuFallback
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ReconciledCapture {
    drawable: Window,
    width: u16,
    height: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OverlaySyncAction {
    None,
    Release,
    Acquire,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct OverlaySyncDecision {
    publish_inhibitor: bool,
    action: OverlaySyncAction,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DamageObject {
    id: damage::Damage,
    drawable: Window,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DamageMode {
    Disabled,
    Ready,
    Active(DamageObject),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DamageSyncAction {
    None,
    Attach(Window),
    Detach,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DamageSubtractQueueAction {
    Fatal,
}

fn damage_subtract_queue_action(_error: &ConnectionError) -> DamageSubtractQueueAction {
    // Failure to queue a request means the connection itself is unusable; X11
    // request rejections arrive later as Event::Error and take the fallback.
    DamageSubtractQueueAction::Fatal
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AsyncX11ErrorAction {
    DisableDamage,
    Fatal,
}

fn async_x11_error_action(extension_name: Option<&str>) -> AsyncX11ErrorAction {
    if extension_name == Some(damage::X11_EXTENSION_NAME) {
        AsyncX11ErrorAction::DisableDamage
    } else {
        AsyncX11ErrorAction::Fatal
    }
}

fn damage_sync_action(mode: DamageMode, target: Option<Window>) -> DamageSyncAction {
    match (mode, target) {
        (DamageMode::Disabled, _) | (DamageMode::Ready, None) => DamageSyncAction::None,
        (DamageMode::Ready, Some(drawable)) => DamageSyncAction::Attach(drawable),
        (DamageMode::Active(_), None) => DamageSyncAction::Detach,
        (DamageMode::Active(active), Some(drawable)) if active.drawable == drawable => {
            DamageSyncAction::None
        }
        (DamageMode::Active(_), Some(drawable)) => DamageSyncAction::Attach(drawable),
    }
}

fn damage_requested(source: CaptureSource) -> bool {
    source != CaptureSource::Root
}

fn supports_damage_version(major: u32, minor: u32) -> bool {
    (major, minor) >= (1, 0)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CursorSnapshot {
    Disabled,
    Position(Option<(i32, i32)>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DamageGateDecision {
    Capture,
    NoChange,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DamageGatePrecheck {
    Capture,
    ProbeCursor,
}

#[derive(Clone, Copy, Debug)]
struct DamageGateState {
    dirty: bool,
    last_capture: Option<Instant>,
    last_geometry: Option<(u16, u16)>,
    last_cursor: Option<CursorSnapshot>,
}

impl DamageGateState {
    fn new() -> Self {
        Self {
            dirty: true,
            last_capture: None,
            last_geometry: None,
            last_cursor: None,
        }
    }

    fn invalidate(&mut self) {
        self.dirty = true;
    }

    fn precheck(
        &self,
        now: Instant,
        geometry: (u16, u16),
        cursor_shape_dirty: bool,
    ) -> DamageGatePrecheck {
        let force_due = self.last_capture.is_some_and(|captured| {
            now.saturating_duration_since(captured) >= DAMAGE_FORCE_REFRESH
        });
        if self.dirty
            || self.last_capture.is_none()
            || self.last_geometry != Some(geometry)
            || self.last_cursor.is_none()
            || cursor_shape_dirty
            || force_due
        {
            DamageGatePrecheck::Capture
        } else {
            DamageGatePrecheck::ProbeCursor
        }
    }

    fn decide_cursor(&self, cursor: CursorSnapshot) -> DamageGateDecision {
        if self.last_cursor == Some(cursor) {
            DamageGateDecision::NoChange
        } else {
            DamageGateDecision::Capture
        }
    }

    fn decide_with_cursor_probe<E>(
        &self,
        now: Instant,
        geometry: (u16, u16),
        cursor_shape_dirty: bool,
        probe: impl FnOnce() -> Result<Option<CursorSnapshot>, E>,
    ) -> Result<DamageGateDecision, E> {
        if self.precheck(now, geometry, cursor_shape_dirty) == DamageGatePrecheck::Capture {
            return Ok(DamageGateDecision::Capture);
        }
        Ok(probe()?.map_or(DamageGateDecision::Capture, |cursor| {
            self.decide_cursor(cursor)
        }))
    }

    #[cfg(test)]
    fn decide(
        &self,
        now: Instant,
        geometry: (u16, u16),
        cursor: CursorSnapshot,
        cursor_shape_dirty: bool,
    ) -> DamageGateDecision {
        self.decide_with_cursor_probe(now, geometry, cursor_shape_dirty, || {
            Ok::<_, std::convert::Infallible>(Some(cursor))
        })
        .expect("an infallible cursor probe cannot fail")
    }

    fn subtract_queued(&mut self) {
        self.dirty = false;
    }

    fn capture_failed(&mut self) {
        self.dirty = true;
    }

    fn capture_succeeded(&mut self, now: Instant, geometry: (u16, u16), cursor: CursorSnapshot) {
        // Do not clear dirty here: a DamageNotify drained while readback was
        // in flight must force the following scheduled capture.
        self.last_capture = Some(now);
        self.last_geometry = Some(geometry);
        self.last_cursor = Some(cursor);
    }
}

struct DamageTracker {
    mode: DamageMode,
    gate: DamageGateState,
}

impl DamageTracker {
    fn connect(conn: &RustConnection) -> RemoteResult<Self> {
        let available = match conn.extension_information(damage::X11_EXTENSION_NAME) {
            Ok(available) => available,
            Err(ConnectionError::UnsupportedExtension) => None,
            Err(error) => return Err(error.into()),
        };
        if available.is_none() {
            eprintln!("remote: XDamage unavailable; capturing the overlay every scheduled tick");
            return Ok(Self::disabled());
        }

        let version = match damage::query_version(
            conn,
            DAMAGE_CLIENT_VERSION.0,
            DAMAGE_CLIENT_VERSION.1,
        ) {
            Ok(cookie) => match cookie.reply() {
                Ok(version) => version,
                Err(ReplyError::X11Error(error)) => {
                    eprintln!(
                        "remote: XDamage negotiation failed ({error:?}); capturing the overlay every scheduled tick"
                    );
                    return Ok(Self::disabled());
                }
                Err(ReplyError::ConnectionError(ConnectionError::UnsupportedExtension)) => {
                    eprintln!(
                        "remote: XDamage became unavailable; capturing the overlay every scheduled tick"
                    );
                    return Ok(Self::disabled());
                }
                Err(ReplyError::ConnectionError(error)) => return Err(error.into()),
            },
            Err(ConnectionError::UnsupportedExtension) => {
                eprintln!(
                    "remote: XDamage became unavailable; capturing the overlay every scheduled tick"
                );
                return Ok(Self::disabled());
            }
            Err(error) => return Err(error.into()),
        };
        if !supports_damage_version(version.major_version, version.minor_version) {
            eprintln!(
                "remote: XDamage {}.{} is too old; capturing the overlay every scheduled tick",
                version.major_version, version.minor_version
            );
            return Ok(Self::disabled());
        }
        Ok(Self {
            mode: DamageMode::Ready,
            gate: DamageGateState::new(),
        })
    }

    fn disabled() -> Self {
        Self {
            mode: DamageMode::Disabled,
            gate: DamageGateState::new(),
        }
    }

    fn active(&self) -> Option<DamageObject> {
        match self.mode {
            DamageMode::Active(active) => Some(active),
            DamageMode::Disabled | DamageMode::Ready => None,
        }
    }

    fn is_active(&self) -> bool {
        self.active().is_some()
    }

    fn invalidate(&mut self) {
        self.gate.invalidate();
    }

    fn notification_matches(&self, event: &damage::NotifyEvent) -> bool {
        self.active()
            .is_some_and(|active| event.damage == active.id && event.drawable == active.drawable)
    }

    fn observe_notification(&mut self, event: &damage::NotifyEvent) {
        if self.notification_matches(event) {
            self.gate.invalidate();
        }
    }

    fn decide_with_cursor_probe<E>(
        &self,
        now: Instant,
        geometry: (u16, u16),
        cursor_shape_dirty: bool,
        probe: impl FnOnce() -> Result<Option<CursorSnapshot>, E>,
    ) -> Result<DamageGateDecision, E> {
        self.gate
            .decide_with_cursor_probe(now, geometry, cursor_shape_dirty, probe)
    }

    fn sync_target(&mut self, conn: &RustConnection, target: Option<Window>) -> RemoteResult<()> {
        match damage_sync_action(self.mode, target) {
            DamageSyncAction::None => Ok(()),
            DamageSyncAction::Detach => self.detach_checked(conn),
            DamageSyncAction::Attach(drawable) => self.attach_checked(conn, drawable),
        }
    }

    fn attach_checked(&mut self, conn: &RustConnection, drawable: Window) -> RemoteResult<()> {
        let id = match conn.generate_id() {
            Ok(id) => id,
            Err(ReplyOrIdError::ConnectionError(ConnectionError::UnsupportedExtension))
            | Err(ReplyOrIdError::IdsExhausted) => {
                self.disable_recoverable(conn, "could not allocate an XDamage resource")?;
                return Ok(());
            }
            Err(ReplyOrIdError::X11Error(error)) => {
                let message = format!("XDamage resource allocation failed: {error:?}");
                self.disable_recoverable(conn, &message)?;
                return Ok(());
            }
            Err(ReplyOrIdError::ConnectionError(error)) => return Err(error.into()),
        };
        let create = match damage::create(conn, id, drawable, damage::ReportLevel::NON_EMPTY) {
            Ok(create) => create,
            Err(ConnectionError::UnsupportedExtension) => {
                self.disable_recoverable(conn, "XDamage became unavailable while attaching")?;
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        };
        match create.check() {
            Ok(()) => {}
            Err(ReplyError::X11Error(error)) => {
                let message = format!("XDamage attach request failed: {error:?}");
                self.disable_recoverable(conn, &message)?;
                return Ok(());
            }
            Err(ReplyError::ConnectionError(ConnectionError::UnsupportedExtension)) => {
                self.disable_recoverable(conn, "XDamage became unavailable while attaching")?;
                return Ok(());
            }
            Err(ReplyError::ConnectionError(error)) => return Err(error.into()),
        }

        let previous = std::mem::replace(
            &mut self.mode,
            DamageMode::Active(DamageObject { id, drawable }),
        );
        self.gate = DamageGateState::new();
        if let DamageMode::Active(previous) = previous {
            match destroy_damage_checked(conn, previous.id) {
                Ok(()) => {}
                Err(DamageDestroyError::Recoverable(message)) => {
                    self.disable_recoverable(conn, &message)?;
                }
                Err(DamageDestroyError::Fatal(error)) => return Err(error),
            }
        }
        Ok(())
    }

    fn detach_checked(&mut self, conn: &RustConnection) -> RemoteResult<()> {
        let previous = std::mem::replace(&mut self.mode, DamageMode::Ready);
        self.gate = DamageGateState::new();
        let DamageMode::Active(previous) = previous else {
            return Ok(());
        };
        match destroy_damage_checked(conn, previous.id) {
            Ok(()) => Ok(()),
            Err(DamageDestroyError::Recoverable(message)) => {
                self.mode = DamageMode::Disabled;
                eprintln!(
                    "remote: XDamage stopped ({message}); capturing the overlay every scheduled tick"
                );
                Ok(())
            }
            Err(DamageDestroyError::Fatal(error)) => Err(error),
        }
    }

    fn prepare_capture(&mut self, conn: &RustConnection) -> RemoteResult<()> {
        let Some(active) = self.active() else {
            return Ok(());
        };
        let subtract = match damage::subtract(conn, active.id, x11rb::NONE, x11rb::NONE) {
            Ok(subtract) => subtract,
            Err(error) => match damage_subtract_queue_action(&error) {
                DamageSubtractQueueAction::Fatal => return Err(error.into()),
            },
        };
        // Ordinary cookie drop keeps request errors in the connection's event
        // stream; never call ignore_error here. The later synchronous image
        // reply is an ordering barrier, and finish_cursor drains any Damage
        // rejection before this frame can be published.
        drop(subtract);
        self.gate.subtract_queued();
        Ok(())
    }

    fn capture_failed(&mut self) {
        if self.is_active() {
            self.gate.capture_failed();
        }
    }

    fn capture_succeeded(&mut self, now: Instant, geometry: (u16, u16), cursor: CursorSnapshot) {
        if self.is_active() {
            self.gate.capture_succeeded(now, geometry, cursor);
        }
    }

    fn disable_recoverable(&mut self, conn: &RustConnection, reason: &str) -> RemoteResult<()> {
        if self.mode == DamageMode::Disabled {
            return Ok(());
        }
        let active = self.take_active_for_cleanup();
        self.gate = DamageGateState::new();
        if let Some(active) = active {
            match destroy_damage_checked(conn, active.id) {
                Ok(()) | Err(DamageDestroyError::Recoverable(_)) => {}
                Err(DamageDestroyError::Fatal(error)) => return Err(error),
            }
        }
        eprintln!("remote: XDamage stopped ({reason}); capturing the overlay every scheduled tick");
        Ok(())
    }

    fn release_best_effort(&mut self, conn: &RustConnection) {
        if let Some(active) = self.take_active_for_cleanup() {
            destroy_damage_best_effort(conn, active.id);
        }
    }

    fn take_active_for_cleanup(&mut self) -> Option<DamageObject> {
        match std::mem::replace(&mut self.mode, DamageMode::Disabled) {
            DamageMode::Active(active) => Some(active),
            DamageMode::Disabled | DamageMode::Ready => None,
        }
    }
}

enum DamageDestroyError {
    Recoverable(String),
    Fatal(RemoteError),
}

fn destroy_damage_checked(
    conn: &RustConnection,
    damage_id: damage::Damage,
) -> Result<(), DamageDestroyError> {
    let destroy = match damage::destroy(conn, damage_id) {
        Ok(destroy) => destroy,
        Err(ConnectionError::UnsupportedExtension) => {
            return Err(DamageDestroyError::Recoverable(
                "XDamage became unavailable while detaching".into(),
            ));
        }
        Err(error) => return Err(DamageDestroyError::Fatal(error.into())),
    };
    match destroy.check() {
        Ok(()) => Ok(()),
        Err(ReplyError::X11Error(error)) => Err(DamageDestroyError::Recoverable(format!(
            "XDamage detach request failed: {error:?}"
        ))),
        Err(ReplyError::ConnectionError(ConnectionError::UnsupportedExtension)) => Err(
            DamageDestroyError::Recoverable("XDamage became unavailable while detaching".into()),
        ),
        Err(ReplyError::ConnectionError(error)) => Err(DamageDestroyError::Fatal(error.into())),
    }
}

fn destroy_damage_best_effort(conn: &RustConnection, damage_id: damage::Damage) {
    if let Ok(cookie) = damage::destroy(conn, damage_id) {
        let _ = cookie.check();
    }
}

struct CapturedDrawable {
    frame: CapturedFrame,
    cursor: CursorSnapshot,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CursorFailureKind {
    Recoverable,
    Fatal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CursorProbeFailureAction {
    DisableAndCapture,
    Fatal,
}

fn cursor_probe_failure_action(kind: CursorFailureKind) -> CursorProbeFailureAction {
    match kind {
        CursorFailureKind::Recoverable => CursorProbeFailureAction::DisableAndCapture,
        CursorFailureKind::Fatal => CursorProbeFailureAction::Fatal,
    }
}

struct CursorFailure {
    kind: CursorFailureKind,
    error: RemoteError,
}

impl CursorFailure {
    fn extension_connection(error: ConnectionError) -> Self {
        let kind = if matches!(error, ConnectionError::UnsupportedExtension) {
            CursorFailureKind::Recoverable
        } else {
            CursorFailureKind::Fatal
        };
        Self {
            kind,
            error: error.into(),
        }
    }

    fn core_connection(error: ConnectionError) -> Self {
        Self {
            kind: CursorFailureKind::Fatal,
            error: error.into(),
        }
    }

    fn core_reply(error: ReplyError) -> Self {
        match error {
            ReplyError::X11Error(error) => Self {
                kind: CursorFailureKind::Recoverable,
                error: io::Error::other(format!("X11 cursor request failed: {error:?}")).into(),
            },
            ReplyError::ConnectionError(error) => Self::core_connection(error),
        }
    }

    fn extension_reply(error: ReplyError) -> Self {
        match error {
            ReplyError::X11Error(error) => Self {
                kind: CursorFailureKind::Recoverable,
                error: io::Error::other(format!("X11 cursor request failed: {error:?}")).into(),
            },
            ReplyError::ConnectionError(error) => Self::extension_connection(error),
        }
    }

    fn data(error: RemoteError) -> Self {
        Self {
            kind: CursorFailureKind::Recoverable,
            error,
        }
    }
}

impl CaptureFailure {
    fn render(error: RemoteError) -> Self {
        Self {
            kind: CaptureFailureKind::Render,
            error,
        }
    }

    fn readback(error: RemoteError) -> Self {
        Self {
            kind: CaptureFailureKind::Readback,
            error,
        }
    }

    fn fatal(error: RemoteError) -> Self {
        Self {
            kind: CaptureFailureKind::Fatal,
            error,
        }
    }
}

fn overlay_sync_decision(
    requested_source: CaptureSource,
    composite_ready: bool,
    owner: Window,
    overlay_acquired: bool,
    transitioned: bool,
    retry_due: bool,
) -> OverlaySyncDecision {
    let action = if requested_source == CaptureSource::Root || !composite_ready {
        OverlaySyncAction::None
    } else if owner == x11rb::NONE {
        OverlaySyncAction::Release
    } else if !overlay_acquired && (transitioned || retry_due) {
        OverlaySyncAction::Acquire
    } else {
        OverlaySyncAction::None
    };
    OverlaySyncDecision {
        publish_inhibitor: transitioned,
        action,
    }
}

pub struct X11Capture {
    conn: RustConnection,
    screen_num: usize,
    root: Window,
    drawable: Window,
    overlay_acquired: bool,
    next_overlay_retry: Option<Instant>,
    compositor: CompositorTracker,
    composite_ready: bool,
    damage: DamageTracker,
    cursor: CursorCapture,
    root_geometry: RootGeometryCache,
    inhibitor_atom: Atom,
    inhibitor_window: Window,
    requested_source: CaptureSource,
    max_width: u16,
    render_scaler: Option<RenderScaler>,
    shm_readback: ShmReadback,
}

impl X11Capture {
    pub fn connect(
        display: Option<&str>,
        requested_source: CaptureSource,
        max_width: u16,
    ) -> RemoteResult<Self> {
        let (conn, screen_num) = x11rb::connect(display)?;
        let screen = conn
            .setup()
            .roots
            .get(screen_num)
            .ok_or_else(|| invalid_data("X11 selected an unavailable screen"))?;
        let root = screen.root;
        // MIT-SHM requires QueryVersion to complete before any other request
        // from the extension. Segment allocation remains lazy until the first
        // frame establishes the exact native image size.
        let shm_readback = ShmReadback::connect(&conn, screen.root_depth);

        let xfixes_ready = match query_xfixes(&conn) {
            Ok(()) => true,
            Err(error) => {
                eprintln!("remote: XFixes cursor capture unavailable: {error}");
                false
            }
        };
        let cursor = if xfixes_ready {
            CursorCapture::new(select_cursor_events(&conn, root))
        } else {
            CursorCapture::disabled()
        };
        let geometry_event_driven = select_geometry_events(&conn, root);
        // Subscribe before taking the authoritative baseline. Any resize that
        // races this reply remains queued and invalidates it before capture.
        let geometry = conn.get_geometry(root)?.reply()?;
        let root_geometry =
            RootGeometryCache::new(geometry.width, geometry.height, geometry_event_driven);
        let composite_ready = if requested_source == CaptureSource::Root {
            false
        } else {
            match query_composite_overlay(&conn) {
                Ok(()) => true,
                Err(error) if requested_source == CaptureSource::Auto => {
                    eprintln!(
                        "remote: X Composite overlay unavailable ({error}); using root capture"
                    );
                    false
                }
                Err(error) => return Err(error),
            }
        };
        // Every source tracks the compositor-manager epoch. A newly started
        // compositor does not receive an old capture-inhibitor PropertyNotify,
        // so even Root capture must republish it after an owner transition.
        let selection = compositor_selection(&conn, screen_num)?;
        let event_driven = xfixes_ready && select_compositor_events(&conn, root, selection);
        // Selection notifications were enabled first, so a concurrent owner
        // transition is queued and reconciled before the first frame.
        let compositor_owner = conn.get_selection_owner(selection)?.reply()?.owner;
        let compositor = CompositorTracker::new(selection, compositor_owner, event_driven);
        let (drawable, overlay_acquired, next_overlay_retry) = match requested_source {
            CaptureSource::Root => (root, false, None),
            CaptureSource::Auto if !composite_ready || compositor_owner == x11rb::NONE => {
                if composite_ready && compositor_owner == x11rb::NONE {
                    eprintln!("remote: no X11 compositor owner found; using root capture");
                }
                (root, false, None)
            }
            CaptureSource::Overlay if compositor_owner == x11rb::NONE => {
                return Err(invalid_data("no X11 compositor owns this screen").into());
            }
            CaptureSource::Auto => match acquire_overlay(&conn, root) {
                Ok(overlay) => (overlay, true, None),
                Err(error) => {
                    eprintln!(
                        "remote: compositor overlay unavailable ({error}); using root capture and retrying"
                    );
                    (root, false, Some(Instant::now() + OVERLAY_RETRY_DELAY))
                }
            },
            CaptureSource::Overlay => (acquire_overlay(&conn, root)?, true, None),
        };
        let (inhibitor_atom, inhibitor_window) = match install_capture_inhibitor(&conn, root) {
            Ok(inhibitor) => inhibitor,
            Err(error) => {
                if overlay_acquired {
                    let _ = composite::release_overlay_window(&conn, root);
                    let _ = conn.flush();
                }
                return Err(error);
            }
        };
        let render_scaler = if max_width == 0 {
            None
        } else {
            match RenderScaler::connect(&conn, screen_num, screen) {
                Ok(scaler) => Some(scaler),
                Err(error) => {
                    eprintln!(
                        "jwm-remote: XRender downscaling unavailable ({error}); using CPU fallback"
                    );
                    None
                }
            }
        };
        let damage = if !damage_requested(requested_source) {
            // Root capture cannot use drawable Damage reliably and must not
            // make an unused optional extension part of session setup.
            DamageTracker::disabled()
        } else {
            DamageTracker::connect(&conn)?
        };

        let mut capture = Self {
            conn,
            screen_num,
            root,
            drawable,
            overlay_acquired,
            next_overlay_retry,
            compositor,
            composite_ready,
            damage,
            cursor,
            root_geometry,
            inhibitor_atom,
            inhibitor_window,
            requested_source,
            max_width,
            render_scaler,
            shm_readback,
        };
        // Drain notifications queued between subscription and the baselines.
        // No pixels are captured until all three caches have been reconciled.
        capture.drain_dynamic_events()?;
        capture.sync_overlay_source()?;
        let damage_target = capture.damage_target();
        capture.damage.sync_target(&capture.conn, damage_target)?;
        Ok(capture)
    }

    /// Capture and optionally downscale a frame.
    ///
    /// `GetImage` is synchronous, but this process is deliberately separate
    /// from JWM.  The compositor event loop is never made to wait for JPEG or
    /// network I/O. The synchronous server readback can still contend with
    /// the compositor on very large roots, which is why the MVP exposes a
    /// conservative frame-rate default.
    pub fn frame(&mut self) -> RemoteResult<CaptureOutcome> {
        self.drain_dynamic_events()?;
        if self.sync_overlay_source()? {
            self.damage.invalidate();
        }
        let (mut source_width, mut source_height) =
            self.root_geometry.dimensions(&self.conn, self.root)?;
        validate_root_geometry(source_width, source_height)?;
        let mut source_geometry_epoch = self.root_geometry.epoch();

        if self.damage.is_active() {
            let decision = match self.damage.decide_with_cursor_probe(
                Instant::now(),
                (source_width, source_height),
                self.cursor.needs_shape(),
                || self.cursor.gate_snapshot(&self.conn, self.root),
            ) {
                Ok(decision) => decision,
                Err(failure) => match cursor_probe_failure_action(failure.kind) {
                    CursorProbeFailureAction::DisableAndCapture => {
                        eprintln!("remote: cursor capture stopped: {}", failure.error);
                        self.cursor.disable();
                        self.damage.invalidate();
                        DamageGateDecision::Capture
                    }
                    CursorProbeFailureAction::Fatal => return Err(failure.error),
                },
            };
            if decision == DamageGateDecision::NoChange {
                return Ok(CaptureOutcome::NoChange);
            }
        }

        let mut drawable = self.drawable;
        let mut allow_render = true;
        let mut dynamic_retry_available = true;
        let mut root_staging_retry_available = true;
        loop {
            // Subtract only after this tick has committed to a fresh readback.
            // A concurrent notify is drained by finish_cursor and re-dirties
            // the gate before the successful baseline is committed.
            self.damage.prepare_capture(&self.conn)?;
            match self.capture_drawable(
                drawable,
                source_width,
                source_height,
                source_geometry_epoch,
                allow_render,
            ) {
                Ok(CaptureDrawableOutcome::Frame(captured)) => {
                    self.damage.capture_succeeded(
                        Instant::now(),
                        (source_width, source_height),
                        captured.cursor,
                    );
                    return Ok(CaptureOutcome::Frame(captured.frame));
                }
                Ok(CaptureDrawableOutcome::RootGeometryChanged) => {
                    self.damage.capture_failed();
                    let attempted_drawable = drawable;
                    self.sync_overlay_source_with_force(true)?;
                    let (width, height) = self
                        .root_geometry
                        .refresh_authoritative(&self.conn, self.root)?;
                    validate_root_geometry(width, height)?;
                    drawable = self.drawable;
                    source_width = width;
                    source_height = height;
                    source_geometry_epoch = self.root_geometry.epoch();
                    match root_snapshot_race_action(
                        root_staging_retry_available,
                        drawable != attempted_drawable,
                    ) {
                        RootSnapshotRaceAction::Retry => {
                            root_staging_retry_available = false;
                            continue;
                        }
                        RootSnapshotRaceAction::CpuFallback => {
                            // Repeated resize/topology churn must remain
                            // bounded. A direct readback is slower but gives
                            // this tick a final compatibility path without
                            // discarding the scaler for later stable frames.
                            allow_render = false;
                        }
                    }
                }
                Err(failure) if failure.kind == CaptureFailureKind::Fatal => {
                    self.damage.capture_failed();
                    return Err(failure.error);
                }
                Err(failure) => {
                    self.damage.capture_failed();
                    if dynamic_retry_available {
                        if let Some(reconciled) = self.reconcile_after_capture_failure(
                            drawable,
                            source_width,
                            source_height,
                            source_geometry_epoch,
                        )? {
                            dynamic_retry_available = false;
                            drawable = reconciled.drawable;
                            source_width = reconciled.width;
                            source_height = reconciled.height;
                            source_geometry_epoch = self.root_geometry.epoch();
                            validate_root_geometry(source_width, source_height)?;
                            continue;
                        }
                    }

                    match failure.kind {
                        CaptureFailureKind::Render => {
                            eprintln!(
                                "jwm-remote: XRender downscaling stopped ({}); using CPU fallback",
                                failure.error
                            );
                            self.release_render_scaler();
                            allow_render = false;
                        }
                        CaptureFailureKind::Readback
                            if self.overlay_acquired
                                && self.requested_source == CaptureSource::Auto =>
                        {
                            eprintln!(
                                "remote: compositor overlay readback failed ({}); using root capture and retrying",
                                failure.error
                            );
                            self.release_overlay_runtime()?;
                            // Arm the retry exactly as the acquire-failure arm
                            // does. Without it, re-acquisition needs a
                            // compositor-owner transition that never comes
                            // while the same compositor keeps running, so one
                            // transient BadDrawable during a RandR resize
                            // downgraded the session to ungated root capture
                            // for life -- and because the damage target is
                            // derived from the overlay, it killed the damage
                            // gate with it.
                            self.next_overlay_retry = Some(Instant::now() + OVERLAY_RETRY_DELAY);
                            drawable = self.root;
                            allow_render = false;
                        }
                        CaptureFailureKind::Readback => return Err(failure.error),
                        CaptureFailureKind::Fatal => unreachable!(),
                    }
                }
            }
        }
    }

    fn capture_drawable(
        &mut self,
        drawable: Window,
        source_width: u16,
        source_height: u16,
        source_geometry_epoch: u64,
        allow_render: bool,
    ) -> Result<CaptureDrawableOutcome, CaptureFailure> {
        let (output_width, output_height) =
            scaled_dimensions(source_width, source_height, self.max_width);
        let render_source = render_capture_source(self.root, drawable);
        let render_allowed_for_source = self
            .render_scaler
            .as_ref()
            .is_some_and(|scaler| scaler.can_capture(render_source, source_width, source_height));
        if (output_width != source_width || output_height != source_height)
            && self.render_scaler.is_some()
            && allow_render
            && render_allowed_for_source
        {
            let pending_cursor = prepare_cursor_for_frame(&self.conn, self.root, &mut self.cursor)?;
            let render_result = self
                .render_scaler
                .as_mut()
                .expect("Render scaler presence checked above")
                .capture(
                    &self.conn,
                    &mut self.shm_readback,
                    render_source,
                    source_width,
                    source_height,
                    output_width,
                    output_height,
                );
            match render_result {
                Ok(mut image) => {
                    let position = resolve_cursor_for_frame(
                        &self.conn,
                        self.root,
                        &mut self.cursor,
                        pending_cursor,
                    )?;
                    self.finish_cursor(position, &mut image, source_width, source_height)
                        .map_err(CaptureFailure::fatal)?;
                    if render_source == RenderCaptureSource::RootSnapshot
                        && !self
                            .root_geometry
                            .root_snapshot_is_current(
                                &self.conn,
                                self.root,
                                source_geometry_epoch,
                                (source_width, source_height),
                            )
                            .map_err(CaptureFailure::fatal)?
                    {
                        return Ok(CaptureDrawableOutcome::RootGeometryChanged);
                    }
                    return Ok(CaptureDrawableOutcome::Frame(CapturedDrawable {
                        cursor: captured_cursor_snapshot(&self.cursor, position),
                        frame: CapturedFrame {
                            image,
                            source_width,
                            source_height,
                        },
                    }));
                }
                Err(error) => {
                    pending_cursor.discard();
                    return Err(CaptureFailure::render(error));
                }
            }
        }

        let root_depth = self.screen().map_err(CaptureFailure::fatal)?.root_depth;
        let pending_cursor = prepare_cursor_for_frame(&self.conn, self.root, &mut self.cursor)?;
        let image_result = self.shm_readback.capture_rgb(
            &self.conn,
            drawable,
            source_width,
            source_height,
            ReadbackLayout::ReplyVisual {
                screen_num: self.screen_num,
                expected_depth: root_depth,
            },
        );
        let mut image = match image_result {
            Ok(image) => image,
            Err(error) => {
                pending_cursor.discard();
                return Err(CaptureFailure::readback(error));
            }
        };
        let position =
            resolve_cursor_for_frame(&self.conn, self.root, &mut self.cursor, pending_cursor)?;
        self.finish_cursor(position, &mut image, source_width, source_height)
            .map_err(CaptureFailure::fatal)?;
        let image = if output_width == source_width && output_height == source_height {
            image
        } else {
            image::imageops::resize(
                &image,
                u32::from(output_width),
                u32::from(output_height),
                FilterType::Triangle,
            )
        };

        Ok(CaptureDrawableOutcome::Frame(CapturedDrawable {
            cursor: captured_cursor_snapshot(&self.cursor, position),
            frame: CapturedFrame {
                image,
                source_width,
                source_height,
            },
        }))
    }

    fn finish_cursor(
        &mut self,
        position: Option<(i32, i32)>,
        image: &mut RgbImage,
        source_width: u16,
        source_height: u16,
    ) -> RemoteResult<()> {
        // A cursor can change while the synchronous frame readback is in
        // flight. Consume the notification before using the cached sprite; a
        // serial mismatch suppresses this frame and refreshes on the next one.
        self.drain_dynamic_events()?;
        if let Some((x, y)) = position {
            self.cursor
                .composite_at(image, x, y, source_width, source_height);
        }
        Ok(())
    }

    fn screen(&self) -> RemoteResult<&Screen> {
        self.conn
            .setup()
            .roots
            .get(self.screen_num)
            .ok_or_else(|| invalid_data("X11 screen disappeared").into())
    }

    fn sync_overlay_source(&mut self) -> RemoteResult<bool> {
        self.sync_overlay_source_with_force(false)
    }

    fn sync_overlay_source_with_force(&mut self, force: bool) -> RemoteResult<bool> {
        let transitioned = self.compositor.refresh(&self.conn, force)?;
        let owner = self.compositor.owner();
        let retry_due = self
            .next_overlay_retry
            .is_some_and(|deadline| Instant::now() >= deadline);
        if !transitioned && !retry_due {
            return Ok(false);
        }
        let decision = overlay_sync_decision(
            self.requested_source,
            self.composite_ready,
            owner,
            self.overlay_acquired,
            transitioned,
            retry_due,
        );
        if decision.publish_inhibitor {
            self.publish_capture_inhibitor()?;
        }
        // The Composite overlay is a screen-level server resource. A direct
        // non-NONE owner handoff does not invalidate an already-held overlay.
        let was_overlay_acquired = self.overlay_acquired;
        match decision.action {
            OverlaySyncAction::None => {}
            OverlaySyncAction::Release => {
                self.next_overlay_retry = None;
                self.release_overlay_runtime()?;
                if self.requested_source == CaptureSource::Overlay {
                    return Err(invalid_data("X11 compositor stopped during remote capture").into());
                }
            }
            OverlaySyncAction::Acquire => match acquire_overlay(&self.conn, self.root) {
                Ok(overlay) => {
                    self.drawable = overlay;
                    self.overlay_acquired = true;
                    self.next_overlay_retry = None;
                    self.damage.sync_target(&self.conn, Some(overlay))?;
                }
                Err(error) if self.requested_source == CaptureSource::Auto => {
                    eprintln!(
                        "remote: compositor overlay unavailable ({error}); using root capture and retrying"
                    );
                    self.drawable = self.root;
                    self.next_overlay_retry = Some(Instant::now() + OVERLAY_RETRY_DELAY);
                }
                Err(error) => return Err(error),
            },
        }
        Ok(transitioned || self.overlay_acquired != was_overlay_acquired)
    }

    fn reconcile_after_capture_failure(
        &mut self,
        attempted_drawable: Window,
        attempted_width: u16,
        attempted_height: u16,
        attempted_geometry_epoch: u64,
    ) -> RemoteResult<Option<ReconciledCapture>> {
        self.drain_dynamic_events()?;
        let source_transition = self.sync_overlay_source_with_force(true)?;
        let (width, height) = self
            .root_geometry
            .refresh_authoritative(&self.conn, self.root)?;
        let changed = source_transition
            || self.drawable != attempted_drawable
            || width != attempted_width
            || height != attempted_height
            || self.root_geometry.epoch() != attempted_geometry_epoch;
        Ok(changed.then_some(ReconciledCapture {
            drawable: self.drawable,
            width,
            height,
        }))
    }

    fn drain_dynamic_events(&mut self) -> RemoteResult<()> {
        while let Some(event) = self.conn.poll_for_event()? {
            match event {
                Event::ConfigureNotify(event)
                    if event.event == self.root && event.window == self.root =>
                {
                    self.root_geometry.invalidate();
                    // A rotation/topology epoch may retain the same WxH.
                    self.damage.invalidate();
                }
                Event::RandrScreenChangeNotify(event) if event.root == self.root => {
                    // RandR reports the unrotated screen size in this event.
                    // Treat it only as an invalidator and query root geometry.
                    self.root_geometry.invalidate();
                    self.damage.invalidate();
                }
                Event::XfixesCursorNotify(event) if event.window == self.root => {
                    self.cursor.observe_serial(event.cursor_serial);
                }
                Event::XfixesSelectionNotify(event) if event.window == self.root => {
                    if event.selection == self.compositor.selection() {
                        self.compositor.invalidate();
                    }
                }
                Event::DamageNotify(event) => self.damage.observe_notification(&event),
                Event::Error(error) => {
                    match async_x11_error_action(error.extension_name.as_deref()) {
                        AsyncX11ErrorAction::DisableDamage => {
                            let message = format!("asynchronous XDamage request failed: {error:?}");
                            self.damage.disable_recoverable(&self.conn, &message)?;
                        }
                        AsyncX11ErrorAction::Fatal => {
                            return Err(io::Error::other(format!(
                                "asynchronous X11 error while capturing: {error:?}"
                            ))
                            .into());
                        }
                    }
                }
                Event::Unknown(_) => {
                    self.damage.disable_recoverable(
                        &self.conn,
                        "an unrecognized extension event made damage tracking unreliable",
                    )?;
                    let geometry = self.root_geometry.fall_back_to_polling();
                    let cursor = self.cursor.fall_back_to_polling();
                    let compositor = self.compositor.fall_back_to_polling();
                    if geometry || cursor || compositor {
                        eprintln!(
                            "remote: unrecognized X11 event disabled event-driven capture caches"
                        );
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn damage_target(&self) -> Option<Window> {
        self.overlay_acquired.then_some(self.drawable)
    }

    fn release_overlay_runtime(&mut self) -> RemoteResult<()> {
        // Stop Damage notifications while the overlay drawable is still
        // owned. Composite ReleaseOverlayWindow always comes afterwards.
        self.damage.sync_target(&self.conn, None)?;
        if let Some(scaler) = self.render_scaler.as_mut() {
            scaler.release_source(&self.conn);
        }
        if self.overlay_acquired {
            let _ = composite::release_overlay_window(&self.conn, self.root);
            let _ = self.conn.flush();
            self.overlay_acquired = false;
        }
        self.drawable = self.root;
        Ok(())
    }

    fn release_overlay_cleanup(&mut self) {
        self.damage.release_best_effort(&self.conn);
        if let Some(scaler) = self.render_scaler.as_mut() {
            scaler.release_source(&self.conn);
        }
        if self.overlay_acquired {
            let _ = composite::release_overlay_window(&self.conn, self.root);
            let _ = self.conn.flush();
            self.overlay_acquired = false;
        }
        self.drawable = self.root;
    }

    fn release_render_scaler(&mut self) {
        if let Some(mut scaler) = self.render_scaler.take() {
            scaler.release(&self.conn);
            let _ = self.conn.flush();
        }
    }

    fn publish_capture_inhibitor(&self) -> RemoteResult<()> {
        self.conn
            .change_property32(
                PropMode::REPLACE,
                self.root,
                self.inhibitor_atom,
                AtomEnum::WINDOW,
                &[self.inhibitor_window],
            )?
            .check()?;
        self.conn.flush()?;
        Ok(())
    }
}

fn prepare_cursor_for_frame<'a>(
    conn: &'a RustConnection,
    root: Window,
    cursor: &mut CursorCapture,
) -> Result<PendingCursor<'a>, CaptureFailure> {
    match cursor.prepare(conn, root) {
        Ok(pending) => Ok(pending),
        Err(failure) if failure.kind == CursorFailureKind::Recoverable => {
            eprintln!("remote: cursor capture stopped: {}", failure.error);
            cursor.disable();
            Ok(PendingCursor::Disabled)
        }
        Err(failure) => Err(CaptureFailure::fatal(failure.error)),
    }
}

fn resolve_cursor_for_frame(
    conn: &RustConnection,
    root: Window,
    cursor: &mut CursorCapture,
    pending: PendingCursor<'_>,
) -> Result<Option<(i32, i32)>, CaptureFailure> {
    match cursor.resolve(conn, root, pending) {
        Ok(position) => Ok(position),
        Err(failure) if failure.kind == CursorFailureKind::Recoverable => {
            eprintln!("remote: cursor capture stopped: {}", failure.error);
            cursor.disable();
            Ok(None)
        }
        Err(failure) => Err(CaptureFailure::fatal(failure.error)),
    }
}

fn captured_cursor_snapshot(
    cursor: &CursorCapture,
    position: Option<(i32, i32)>,
) -> CursorSnapshot {
    if cursor.mode == CursorMode::Disabled {
        CursorSnapshot::Disabled
    } else {
        CursorSnapshot::Position(position)
    }
}

struct CompositorTracker {
    selection: Atom,
    owner: Window,
    event_driven: bool,
    dirty: bool,
    saw_transition: bool,
}

impl CompositorTracker {
    fn new(selection: Atom, owner: Window, event_driven: bool) -> Self {
        Self {
            selection,
            owner,
            event_driven,
            dirty: false,
            saw_transition: false,
        }
    }

    fn selection(&self) -> Atom {
        self.selection
    }

    fn owner(&self) -> Window {
        self.owner
    }

    fn invalidate(&mut self) {
        self.dirty = true;
        self.saw_transition = true;
    }

    fn fall_back_to_polling(&mut self) -> bool {
        if !self.event_driven {
            return false;
        }
        self.event_driven = false;
        self.dirty = true;
        true
    }

    fn refresh(&mut self, conn: &RustConnection, force: bool) -> RemoteResult<bool> {
        if self.event_driven && !self.dirty && !force {
            return Ok(false);
        }
        let owner = conn.get_selection_owner(self.selection)?.reply()?.owner;
        let changed = owner != self.owner;
        let saw_transition = self.saw_transition;
        self.owner = owner;
        self.dirty = false;
        self.saw_transition = false;
        Ok(changed || saw_transition)
    }
}

struct RootGeometryCache {
    width: u16,
    height: u16,
    event_driven: bool,
    dirty: bool,
    epoch: u64,
}

impl RootGeometryCache {
    fn new(width: u16, height: u16, event_driven: bool) -> Self {
        Self {
            width,
            height,
            event_driven,
            dirty: false,
            epoch: 0,
        }
    }

    fn invalidate(&mut self) {
        self.dirty = true;
        self.epoch = self.epoch.wrapping_add(1);
    }

    fn fall_back_to_polling(&mut self) -> bool {
        if !self.event_driven {
            return false;
        }
        self.event_driven = false;
        self.invalidate();
        true
    }

    fn epoch(&self) -> u64 {
        self.epoch
    }

    fn event_snapshot_is_current(&self, captured_epoch: u64) -> bool {
        !self.dirty && self.epoch == captured_epoch
    }

    fn needs_post_capture_query(&self) -> bool {
        !self.event_driven
    }

    fn dimensions(&mut self, conn: &RustConnection, root: Window) -> RemoteResult<(u16, u16)> {
        if !self.event_driven || self.dirty {
            return self.refresh_authoritative(conn, root);
        }
        Ok((self.width, self.height))
    }

    fn refresh_authoritative(
        &mut self,
        conn: &RustConnection,
        root: Window,
    ) -> RemoteResult<(u16, u16)> {
        let geometry = conn.get_geometry(root)?.reply()?;
        if (geometry.width, geometry.height) != (self.width, self.height) {
            self.epoch = self.epoch.wrapping_add(1);
        }
        self.width = geometry.width;
        self.height = geometry.height;
        self.dirty = false;
        Ok((self.width, self.height))
    }

    fn root_snapshot_is_current(
        &mut self,
        conn: &RustConnection,
        root: Window,
        captured_epoch: u64,
        captured_dimensions: (u16, u16),
    ) -> RemoteResult<bool> {
        if !self.needs_post_capture_query() {
            return Ok(self.event_snapshot_is_current(captured_epoch));
        }

        // Polling is the conservative fallback when neither core nor RandR
        // notifications can be trusted. Query after the synchronous small
        // readback so a resize during CopyArea cannot publish stale edges.
        let dimensions = self.refresh_authoritative(conn, root)?;
        Ok(self.epoch == captured_epoch && dimensions == captured_dimensions)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CursorMode {
    Disabled,
    Polling,
    EventDriven,
}

struct CursorCapture {
    mode: CursorMode,
    dirty: bool,
    pending_serial: Option<u32>,
    shape: Option<CursorShape>,
}

impl CursorCapture {
    fn disabled() -> Self {
        Self {
            mode: CursorMode::Disabled,
            dirty: false,
            pending_serial: None,
            shape: None,
        }
    }

    fn new(event_driven: bool) -> Self {
        Self {
            mode: if event_driven {
                CursorMode::EventDriven
            } else {
                CursorMode::Polling
            },
            dirty: true,
            pending_serial: None,
            shape: None,
        }
    }

    fn disable(&mut self) {
        self.mode = CursorMode::Disabled;
        self.dirty = false;
        self.pending_serial = None;
        self.shape = None;
    }

    fn fall_back_to_polling(&mut self) -> bool {
        if self.mode != CursorMode::EventDriven {
            return false;
        }
        self.mode = CursorMode::Polling;
        self.dirty = true;
        self.pending_serial = None;
        true
    }

    fn observe_serial(&mut self, serial: u32) {
        if self.mode != CursorMode::EventDriven {
            return;
        }
        self.pending_serial = Some(serial);
        self.dirty = self
            .shape
            .as_ref()
            .is_none_or(|shape| shape.serial != serial);
    }

    fn needs_shape(&self) -> bool {
        match self.mode {
            CursorMode::Disabled => false,
            CursorMode::Polling => true,
            CursorMode::EventDriven => self.dirty || self.shape.is_none(),
        }
    }

    fn gate_snapshot(
        &self,
        conn: &RustConnection,
        root: Window,
    ) -> Result<Option<CursorSnapshot>, CursorFailure> {
        match self.mode {
            CursorMode::Disabled => Ok(Some(CursorSnapshot::Disabled)),
            // Polling mode needs a post-readback shape and position query on
            // every frame, so it deliberately keeps the legacy per-tick path.
            CursorMode::Polling => Ok(None),
            CursorMode::EventDriven => {
                let pointer = conn
                    .query_pointer(root)
                    .map_err(CursorFailure::core_connection)?
                    .reply()
                    .map_err(CursorFailure::core_reply)?;
                Ok(Some(CursorSnapshot::Position(pointer_position(
                    root, &pointer,
                ))))
            }
        }
    }

    fn update_shape(&mut self, reply: &xfixes::GetCursorImageReply) -> RemoteResult<()> {
        let pending_serial = self.pending_serial.take();
        let replace = self
            .shape
            .as_ref()
            .is_none_or(|shape| !shape.matches(reply));
        if replace {
            self.shape = Some(CursorShape::from_reply(reply)?);
        }
        self.dirty = self.mode == CursorMode::EventDriven
            && pending_serial.is_some_and(|serial| serial != reply.cursor_serial);
        Ok(())
    }

    fn prepare<'a>(
        &self,
        conn: &'a RustConnection,
        root: Window,
    ) -> Result<PendingCursor<'a>, CursorFailure> {
        match self.mode {
            CursorMode::Disabled => Ok(PendingCursor::Disabled),
            // Without cursor notifications, fetch both position and pixels
            // after readback to preserve the reliable legacy snapshot path.
            CursorMode::Polling => Ok(PendingCursor::Polling),
            CursorMode::EventDriven => {
                let pointer = conn
                    .query_pointer(root)
                    .map_err(CursorFailure::core_connection)?;
                let shape = if self.needs_shape() {
                    match xfixes::get_cursor_image(conn) {
                        Ok(shape) => Some(shape),
                        Err(error) => {
                            pointer.discard_reply_and_errors();
                            return Err(CursorFailure::extension_connection(error));
                        }
                    }
                } else {
                    None
                };
                Ok(PendingCursor::EventDriven { pointer, shape })
            }
        }
    }

    fn resolve(
        &mut self,
        conn: &RustConnection,
        root: Window,
        pending: PendingCursor<'_>,
    ) -> Result<Option<(i32, i32)>, CursorFailure> {
        let pointer = match pending {
            PendingCursor::Disabled => return Ok(None),
            PendingCursor::Polling => {
                let pointer = conn
                    .query_pointer(root)
                    .map_err(CursorFailure::core_connection)?;
                let shape = match xfixes::get_cursor_image(conn) {
                    Ok(shape) => shape,
                    Err(error) => {
                        pointer.discard_reply_and_errors();
                        return Err(CursorFailure::extension_connection(error));
                    }
                };
                let pointer = match pointer.reply() {
                    Ok(pointer) => pointer,
                    Err(error) => {
                        shape.discard_reply_and_errors();
                        return Err(CursorFailure::core_reply(error));
                    }
                };
                let shape = shape.reply().map_err(CursorFailure::extension_reply)?;
                self.update_shape(&shape).map_err(CursorFailure::data)?;
                pointer
            }
            PendingCursor::EventDriven { pointer, shape } => {
                let pointer = match pointer.reply() {
                    Ok(pointer) => pointer,
                    Err(error) => {
                        if let Some(shape) = shape {
                            shape.discard_reply_and_errors();
                        }
                        return Err(CursorFailure::core_reply(error));
                    }
                };
                if let Some(shape) = shape {
                    let shape = shape.reply().map_err(CursorFailure::extension_reply)?;
                    self.update_shape(&shape).map_err(CursorFailure::data)?;
                }
                pointer
            }
        };
        Ok(pointer_position(root, &pointer))
    }

    fn composite_at(
        &mut self,
        image: &mut RgbImage,
        pointer_x: i32,
        pointer_y: i32,
        source_width: u16,
        source_height: u16,
    ) {
        if self.dirty {
            return;
        }
        if let Some(shape) = self.shape.as_mut() {
            shape.composite(image, pointer_x, pointer_y, source_width, source_height);
        }
    }
}

fn pointer_position(root: Window, pointer: &QueryPointerReply) -> Option<(i32, i32)> {
    (pointer.same_screen && pointer.root == root)
        .then_some((i32::from(pointer.root_x), i32::from(pointer.root_y)))
}

enum PendingCursor<'a> {
    Disabled,
    Polling,
    EventDriven {
        pointer: Cookie<'a, RustConnection, QueryPointerReply>,
        shape: Option<Cookie<'a, RustConnection, xfixes::GetCursorImageReply>>,
    },
}

impl PendingCursor<'_> {
    fn discard(self) {
        if let Self::EventDriven { pointer, shape } = self {
            pointer.discard_reply_and_errors();
            if let Some(shape) = shape {
                shape.discard_reply_and_errors();
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CursorScaleKey {
    source_width: u16,
    source_height: u16,
    output_width: u32,
    output_height: u32,
}

struct ScaledCursorShape {
    key: CursorScaleKey,
    image: RgbaImage,
}

struct CursorShape {
    serial: u32,
    width: u16,
    height: u16,
    xhot: u16,
    yhot: u16,
    image: RgbaImage,
    scaled: Option<ScaledCursorShape>,
}

impl CursorShape {
    fn matches(&self, reply: &xfixes::GetCursorImageReply) -> bool {
        self.serial == reply.cursor_serial
            && self.width == reply.width
            && self.height == reply.height
            && self.xhot == reply.xhot
            && self.yhot == reply.yhot
    }

    fn from_reply(reply: &xfixes::GetCursorImageReply) -> RemoteResult<Self> {
        let pixels = usize::from(reply.width)
            .checked_mul(usize::from(reply.height))
            .ok_or_else(|| invalid_data("XFixes cursor dimensions overflow"))?;
        if reply.cursor_image.len() != pixels {
            return Err(invalid_data("XFixes cursor image has an invalid length").into());
        }
        let bytes = pixels
            .checked_mul(4)
            .ok_or_else(|| invalid_data("XFixes cursor buffer size overflow"))?;
        let mut rgba = Vec::new();
        rgba.try_reserve_exact(bytes)
            .map_err(|_| invalid_data("could not allocate the XFixes cursor buffer"))?;
        for argb in reply.cursor_image.iter().copied() {
            rgba.extend_from_slice(&[
                ((argb >> 16) & 0xff) as u8,
                ((argb >> 8) & 0xff) as u8,
                (argb & 0xff) as u8,
                ((argb >> 24) & 0xff) as u8,
            ]);
        }
        let image = RgbaImage::from_raw(u32::from(reply.width), u32::from(reply.height), rgba)
            .ok_or_else(|| invalid_data("XFixes cursor image dimensions are invalid"))?;
        Ok(Self {
            serial: reply.cursor_serial,
            width: reply.width,
            height: reply.height,
            xhot: reply.xhot,
            yhot: reply.yhot,
            image,
            scaled: None,
        })
    }

    fn composite(
        &mut self,
        output: &mut RgbImage,
        pointer_x: i32,
        pointer_y: i32,
        source_width: u16,
        source_height: u16,
    ) {
        if self.width == 0 || self.height == 0 || source_width == 0 || source_height == 0 {
            return;
        }
        if output.width() == u32::from(source_width) && output.height() == u32::from(source_height)
        {
            blend_premultiplied_cursor(
                output,
                &self.image,
                pointer_x - i32::from(self.xhot),
                pointer_y - i32::from(self.yhot),
            );
            return;
        }

        let key = CursorScaleKey {
            source_width,
            source_height,
            output_width: output.width(),
            output_height: output.height(),
        };
        if self.scaled.as_ref().is_none_or(|scaled| scaled.key != key) {
            self.scaled = Some(ScaledCursorShape {
                key,
                image: image::imageops::resize(
                    &self.image,
                    scale_length(self.width, output.width(), source_width),
                    scale_length(self.height, output.height(), source_height),
                    FilterType::Triangle,
                ),
            });
        }
        let Some(scaled) = self.scaled.as_ref() else {
            return;
        };
        blend_premultiplied_cursor(
            output,
            &scaled.image,
            scale_coordinate(
                pointer_x - i32::from(self.xhot),
                output.width(),
                source_width,
            ),
            scale_coordinate(
                pointer_y - i32::from(self.yhot),
                output.height(),
                source_height,
            ),
        );
    }
}

fn blend_premultiplied_cursor(output: &mut RgbImage, cursor: &RgbaImage, left: i32, top: i32) {
    for (cursor_x, cursor_y, source) in cursor.enumerate_pixels() {
        let x = left + cursor_x as i32;
        let y = top + cursor_y as i32;
        if x < 0 || y < 0 || x >= output.width() as i32 || y >= output.height() as i32 {
            continue;
        }
        let alpha = u32::from(source[3]);
        if alpha == 0 {
            continue;
        }
        let inverse = 255 - alpha;
        let pixel = output.get_pixel_mut(x as u32, y as u32);
        for channel in 0..3 {
            pixel[channel] = (u32::from(source[channel])
                + (u32::from(pixel[channel]) * inverse + 127) / 255)
                .min(255) as u8;
        }
    }
}

#[derive(Clone, Copy)]
enum ReadbackLayout {
    /// Windows report their visual in GetImage/ShmGetImage. Keep validating
    /// it instead of assuming that every capture drawable uses the root masks.
    ReplyVisual {
        screen_num: usize,
        expected_depth: u8,
    },
    /// Pixmap GetImage replies have visual NONE. XRender's target pixmap was
    /// explicitly created with the root format, so its layout is already known.
    Known {
        expected_depth: u8,
        layout: PixelLayout,
        standard_bgrx_visual: bool,
    },
}

impl ReadbackLayout {
    fn expected_depth(self) -> u8 {
        match self {
            Self::ReplyVisual { expected_depth, .. } | Self::Known { expected_depth, .. } => {
                expected_depth
            }
        }
    }
}

enum ShmReadback {
    Disabled,
    Enabled(EnabledShmReadback),
}

impl ShmReadback {
    fn connect(conn: &RustConnection, depth: u8) -> Self {
        match Self::try_connect(conn, depth) {
            Ok(Some(readback)) => Self::Enabled(readback),
            Ok(None) => Self::Disabled,
            Err(error) => {
                eprintln!(
                    "jwm-remote: MIT-SHM FD readback unavailable ({error}); using core GetImage"
                );
                Self::Disabled
            }
        }
    }

    fn try_connect(conn: &RustConnection, depth: u8) -> RemoteResult<Option<EnabledShmReadback>> {
        if conn
            .extension_information(shm::X11_EXTENSION_NAME)?
            .is_none()
        {
            eprintln!("jwm-remote: MIT-SHM unavailable; using core GetImage");
            return Ok(None);
        }

        // The protocol requires this reply before any other MIT-SHM request.
        let version = shm::query_version(conn)?.reply()?;
        let negotiated = (version.major_version, version.minor_version);
        if !supports_shm_fd_version(version.major_version, version.minor_version) {
            eprintln!(
                "jwm-remote: MIT-SHM {}.{} lacks 1.2 FD segments; using core GetImage",
                version.major_version, version.minor_version
            );
            return Ok(None);
        }
        let format = conn
            .setup()
            .pixmap_formats
            .iter()
            .find(|format| format.depth == depth)
            .copied()
            .ok_or_else(|| invalid_data(format!("X11 has no pixmap format for depth {depth}")))?;
        // Validate the native format before announcing SHM as available.
        shm_buffer_size(1, 1, format)?;
        Ok(Some(EnabledShmReadback {
            format,
            version: negotiated,
            segment: None,
            reported_active: false,
        }))
    }

    fn capture_rgb(
        &mut self,
        conn: &RustConnection,
        drawable: u32,
        width: u16,
        height: u16,
        layout: ReadbackLayout,
    ) -> RemoteResult<RgbImage> {
        let shm_result = match self {
            Self::Disabled => None,
            Self::Enabled(readback) => {
                Some(readback.capture_rgb(conn, drawable, width, height, layout))
            }
        };
        match resolve_readback(shm_result, || {
            core_capture_rgb(conn, drawable, width, height, layout)
        }) {
            ReadbackOutcome::Image(image) => Ok(image),
            ReadbackOutcome::CoreFallback { image, shm_error } => {
                self.disable(conn);
                eprintln!(
                    "jwm-remote: MIT-SHM FD readback stopped ({shm_error}); using core GetImage"
                );
                Ok(image)
            }
            ReadbackOutcome::Error(error) => Err(error),
        }
    }

    fn disable(&mut self, conn: &RustConnection) {
        if let Self::Enabled(mut readback) = std::mem::replace(self, Self::Disabled) {
            readback.release(conn);
        }
    }

    fn release(&mut self, conn: &RustConnection) {
        self.disable(conn);
    }
}

struct EnabledShmReadback {
    format: Format,
    version: (u16, u16),
    segment: Option<ShmSegment>,
    reported_active: bool,
}

impl EnabledShmReadback {
    fn capture_rgb(
        &mut self,
        conn: &RustConnection,
        drawable: u32,
        width: u16,
        height: u16,
        layout: ReadbackLayout,
    ) -> RemoteResult<RgbImage> {
        let expected_size = shm_buffer_size(width, height, self.format)?;
        self.ensure_capacity(conn, expected_size)?;
        let (image, capacity) = {
            let segment = self
                .segment
                .as_ref()
                .ok_or_else(|| invalid_data("MIT-SHM segment was not created"))?;
            let reply = shm::get_image(
                conn,
                drawable,
                0,
                0,
                width,
                height,
                u32::MAX,
                u8::from(ImageFormat::Z_PIXMAP),
                segment.id,
                0,
            )?
            .reply()?;
            let image_size = validate_shm_reply(
                self.format.depth,
                expected_size,
                segment.mapping.len(),
                reply.depth,
                reply.size,
            )?;
            let bytes = segment.mapping.bytes(image_size)?;
            let ximage = XImage::new(
                width,
                height,
                self.format.scanline_pad.try_into()?,
                reply.depth,
                self.format.bits_per_pixel.try_into()?,
                conn.setup().image_byte_order.try_into()?,
                Cow::Borrowed(bytes),
            )?;
            (
                decode_readback_image(conn, &ximage, width, height, reply.visual, layout)?,
                segment.mapping.len(),
            )
        };
        if !self.reported_active {
            eprintln!(
                "jwm-remote: MIT-SHM {}.{} FD readback active ({capacity} bytes)",
                self.version.0, self.version.1
            );
            self.reported_active = true;
        }
        Ok(image)
    }

    fn ensure_capacity(&mut self, conn: &RustConnection, required: usize) -> RemoteResult<()> {
        let current_capacity = self.segment.as_ref().map(|segment| segment.mapping.len());
        if !shm_needs_growth(current_capacity, required) {
            return Ok(());
        }

        // Build the replacement completely before releasing the old segment.
        // A failed RandR growth therefore leaves the previous resource valid.
        let replacement = ShmSegment::create(conn, required)?;
        let old = self.segment.replace(replacement);
        if let Some(old) = old {
            old.release(conn);
        }
        Ok(())
    }

    fn release(&mut self, conn: &RustConnection) {
        if let Some(segment) = self.segment.take() {
            segment.release(conn);
        }
    }
}

struct ShmSegment {
    id: shm::Seg,
    mapping: MappedRegion,
    // Keeping the owned descriptor makes close ordering explicit. It is
    // closed after the mapping is unmapped when this struct is dropped.
    _file: File,
}

impl ShmSegment {
    fn create(conn: &RustConnection, capacity: usize) -> RemoteResult<Self> {
        let size = u32::try_from(capacity)
            .map_err(|_| invalid_data("MIT-SHM image buffer exceeds the protocol size limit"))?;
        if size == 0 {
            return Err(invalid_data("MIT-SHM image buffer is empty").into());
        }
        let id = conn.generate_id()?;
        // `false` is required: the server writes captured pixels into the segment.
        let cookie = shm::create_segment(conn, id, size, false)?;
        // Split waiting from parsing. An X11/raw transport error does not prove
        // that the server created the resource, so do not detach an uncreated
        // XID. Once a success reply exists, any malformed/missing FD path must
        // detach because the server-side segment is already live.
        let (buffer, mut fds) = cookie.raw_reply()?;
        let reply = match shm::CreateSegmentReply::try_parse_fd(buffer.as_ref(), &mut fds) {
            Ok((reply, _)) => reply,
            Err(error) => {
                detach_shm_segment(conn, id);
                return Err(error.into());
            }
        };
        let result = (|| -> RemoteResult<Self> {
            if reply.nfd != 1 {
                return Err(invalid_data(format!(
                    "MIT-SHM CreateSegment returned {} file descriptors",
                    reply.nfd
                ))
                .into());
            }
            let file = File::from(reply.shm_fd);
            let file_size = file.metadata()?.len();
            if file_size < u64::from(size) {
                return Err(invalid_data(format!(
                    "MIT-SHM segment is shorter than requested: {file_size} < {size}"
                ))
                .into());
            }
            let mapping = MappedRegion::new(&file, capacity)?;
            Ok(Self {
                id,
                mapping,
                _file: file,
            })
        })();
        match result {
            Ok(segment) => Ok(segment),
            Err(error) => {
                // A parsed success reply makes this XID a live server resource.
                detach_shm_segment(conn, id);
                Err(error)
            }
        }
    }

    fn release(self, conn: &RustConnection) {
        // ShmGetImage is synchronous, so no server write is outstanding here.
        // Check Detach before dropping the local mapping and descriptor.
        detach_shm_segment(conn, self.id);
        drop(self);
    }
}

fn detach_shm_segment(conn: &RustConnection, segment: shm::Seg) {
    if let Ok(cookie) = shm::detach(conn, segment) {
        let _ = cookie.check();
    }
}

struct MappedRegion {
    address: NonNull<u8>,
    length: usize,
}

impl MappedRegion {
    fn new(file: &File, length: usize) -> io::Result<Self> {
        if length == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cannot mmap an empty MIT-SHM segment",
            ));
        }
        // SAFETY: `file` is a live CreateSegment descriptor, `length` was
        // checked against fstat, and the mapping is owned until munmap in Drop.
        // The client only reads; CreateSegment(false) gives the server write access.
        let mapped = unsafe {
            libc::mmap(
                ptr::null_mut(),
                length,
                libc::PROT_READ,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if mapped == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let Some(address) = NonNull::new(mapped.cast::<u8>()) else {
            // SAFETY: mmap succeeded and returned this exact address/length.
            unsafe {
                libc::munmap(mapped, length);
            }
            return Err(io::Error::other("MIT-SHM mmap returned a null address"));
        };
        Ok(Self { address, length })
    }

    fn len(&self) -> usize {
        self.length
    }

    fn bytes(&self, length: usize) -> io::Result<&[u8]> {
        if length > self.length {
            return Err(invalid_data(format!(
                "MIT-SHM reply exceeds its mapping: {length} > {}",
                self.length
            )));
        }
        // SAFETY: the mapping is valid for `self.length`, the preceding
        // ShmGetImage reply guarantees the server finished writing, and the
        // returned borrow cannot outlive `self` or overlap the next capture.
        Ok(unsafe { std::slice::from_raw_parts(self.address.as_ptr(), length) })
    }
}

impl Drop for MappedRegion {
    fn drop(&mut self) {
        // SAFETY: this exact address/length pair came from the successful mmap
        // in `new` and is unmapped exactly once here.
        unsafe {
            libc::munmap(self.address.as_ptr().cast(), self.length);
        }
    }
}

enum ReadbackOutcome<T, E> {
    Image(T),
    CoreFallback { image: T, shm_error: E },
    Error(E),
}

fn resolve_readback<T, E>(
    shm_result: Option<Result<T, E>>,
    core: impl FnOnce() -> Result<T, E>,
) -> ReadbackOutcome<T, E> {
    match shm_result {
        None => match core() {
            Ok(image) => ReadbackOutcome::Image(image),
            Err(error) => ReadbackOutcome::Error(error),
        },
        Some(Ok(image)) => ReadbackOutcome::Image(image),
        Some(Err(shm_error)) => match core() {
            Ok(image) => ReadbackOutcome::CoreFallback { image, shm_error },
            // When both paths reject the same drawable, preserve the core
            // error and let the existing overlay/Render lifecycle handle it.
            // Do not globally disable SHM for a transient BadDrawable.
            Err(error) => ReadbackOutcome::Error(error),
        },
    }
}

fn supports_shm_fd_version(major: u16, minor: u16) -> bool {
    (major, minor) >= SHM_FD_VERSION
}

fn shm_needs_growth(current_capacity: Option<usize>, required: usize) -> bool {
    current_capacity.is_none_or(|capacity| capacity < required)
}

fn core_capture_rgb(
    conn: &RustConnection,
    drawable: u32,
    width: u16,
    height: u16,
    layout: ReadbackLayout,
) -> RemoteResult<RgbImage> {
    let reply = conn
        .get_image(
            ImageFormat::Z_PIXMAP,
            drawable,
            0,
            0,
            width,
            height,
            u32::MAX,
        )?
        .reply()?;
    let visual = reply.visual;
    let image = XImage::get_from_reply(conn.setup(), width, height, reply)?;
    decode_readback_image(conn, &image, width, height, visual, layout)
}

fn decode_readback_image(
    conn: &RustConnection,
    image: &XImage<'_>,
    width: u16,
    height: u16,
    visual_id: Visualid,
    layout: ReadbackLayout,
) -> RemoteResult<RgbImage> {
    if image.depth() != layout.expected_depth() {
        return Err(invalid_data(format!(
            "X11 capture depth changed from {} to {}",
            layout.expected_depth(),
            image.depth()
        ))
        .into());
    }
    let (pixel_layout, standard_bgrx_visual) = match layout {
        ReadbackLayout::ReplyVisual { screen_num, .. } => {
            let screen = conn
                .setup()
                .roots
                .get(screen_num)
                .ok_or_else(|| invalid_data("X11 selected an unavailable screen"))?;
            let visual = find_visual(screen, visual_id)
                .ok_or_else(|| invalid_data("X11 capture visual is not described by the screen"))?;
            if visual.class != VisualClass::TRUE_COLOR {
                return Err(invalid_data("remote capture requires an X11 TrueColor visual").into());
            }
            (
                PixelLayout::from_visual_type(visual)?,
                is_standard_bgrx_visual(visual),
            )
        }
        ReadbackLayout::Known {
            layout,
            standard_bgrx_visual,
            ..
        } => (layout, standard_bgrx_visual),
    };
    decode_ximage(image, width, height, pixel_layout, standard_bgrx_visual)
}

fn shm_buffer_size(width: u16, height: u16, format: Format) -> RemoteResult<usize> {
    if width == 0 || height == 0 {
        return Err(invalid_data("MIT-SHM image dimensions must be nonzero").into());
    }
    let bits_per_pixel = usize::from(
        BitsPerPixel::try_from(format.bits_per_pixel)
            .map_err(|_| invalid_data("X11 reported invalid native bits-per-pixel"))?,
    );
    let scanline_pad = usize::from(
        ScanlinePad::try_from(format.scanline_pad)
            .map_err(|_| invalid_data("X11 reported invalid native scanline padding"))?,
    );
    let row_bits = usize::from(width)
        .checked_mul(bits_per_pixel)
        .ok_or_else(|| invalid_data("MIT-SHM scanline size overflow"))?;
    let padded_units = row_bits
        .checked_add(scanline_pad - 1)
        .ok_or_else(|| invalid_data("MIT-SHM scanline padding overflow"))?
        / scanline_pad;
    let padded_bits = padded_units
        .checked_mul(scanline_pad)
        .ok_or_else(|| invalid_data("MIT-SHM padded scanline size overflow"))?;
    let stride = padded_bits / 8;
    let size = stride
        .checked_mul(usize::from(height))
        .ok_or_else(|| invalid_data("MIT-SHM image size overflow"))?;
    u32::try_from(size)
        .map_err(|_| invalid_data("MIT-SHM image exceeds the protocol size limit"))?;
    Ok(size)
}

fn validate_shm_reply(
    expected_depth: u8,
    expected_size: usize,
    mapped_size: usize,
    reply_depth: u8,
    reply_size: u32,
) -> RemoteResult<usize> {
    if reply_depth != expected_depth {
        return Err(invalid_data(format!(
            "MIT-SHM capture depth changed from {expected_depth} to {reply_depth}"
        ))
        .into());
    }
    let reply_size = usize::try_from(reply_size)
        .map_err(|_| invalid_data("MIT-SHM reply size exceeds this platform"))?;
    if reply_size != expected_size {
        return Err(invalid_data(format!(
            "MIT-SHM returned {reply_size} bytes; expected {expected_size}"
        ))
        .into());
    }
    if reply_size > mapped_size {
        return Err(invalid_data(format!(
            "MIT-SHM reply exceeds its mapping: {reply_size} > {mapped_size}"
        ))
        .into());
    }
    Ok(reply_size)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RenderCaptureSource {
    Overlay(Window),
    RootSnapshot,
}

fn render_capture_source(root: Window, drawable: Window) -> RenderCaptureSource {
    if drawable == root {
        RenderCaptureSource::RootSnapshot
    } else {
        RenderCaptureSource::Overlay(drawable)
    }
}

fn root_staging_bytes(width: u16, height: u16, format: Format) -> Option<usize> {
    shm_buffer_size(width, height, format)
        .ok()
        .filter(|size| *size <= ROOT_STAGING_MAX_BYTES)
}

fn resolve_render_readback<T, E>(
    image: Result<T, E>,
    copy: Result<(), E>,
    composite: Result<(), E>,
) -> Result<T, E> {
    copy?;
    composite?;
    image
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RootStagingReplacement {
    PreserveOld,
    ReleaseOld,
}

fn root_staging_replacement(old_bytes: Option<usize>, new_bytes: usize) -> RootStagingReplacement {
    if old_bytes.is_some_and(|old| old.saturating_add(new_bytes) > ROOT_STAGING_MAX_BYTES) {
        RootStagingReplacement::ReleaseOld
    } else {
        RootStagingReplacement::PreserveOld
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RenderSourceBacking {
    Overlay {
        drawable: Window,
    },
    RootSnapshot {
        pixmap: Pixmap,
        gc: Gcontext,
        width: u16,
        height: u16,
        bytes: usize,
    },
}

struct RenderSource {
    backing: RenderSourceBacking,
    picture: Picture,
    transform: Option<(u16, u16, u16, u16)>,
}

struct RenderTarget {
    pixmap: Pixmap,
    picture: Picture,
    width: u16,
    height: u16,
}

struct RenderScaler {
    visual_formats: Vec<(Visualid, Pictformat)>,
    root: Window,
    root_depth: u8,
    root_format: Pictformat,
    root_pixmap_format: Format,
    root_layout: PixelLayout,
    root_standard_bgrx_visual: bool,
    source: Option<RenderSource>,
    target: Option<RenderTarget>,
    reported_dimensions: Option<(u16, u16, u16, u16)>,
}

impl RenderScaler {
    fn connect(conn: &RustConnection, screen_num: usize, screen: &Screen) -> RemoteResult<Self> {
        let version =
            render::query_version(conn, RENDER_CLIENT_VERSION.0, RENDER_CLIENT_VERSION.1)?
                .reply()?;
        if (version.major_version, version.minor_version) < RENDER_TRANSFORM_VERSION {
            return Err(invalid_data(format!(
                "XRender {}.{} is too old; server-side scaling requires {}.{}",
                version.major_version,
                version.minor_version,
                RENDER_TRANSFORM_VERSION.0,
                RENDER_TRANSFORM_VERSION.1
            ))
            .into());
        }

        let formats = render::query_pict_formats(conn)?.reply()?;
        let render_screen = formats
            .screens
            .get(screen_num)
            .ok_or_else(|| invalid_data("XRender did not describe the selected X11 screen"))?;
        let visual_formats: Vec<_> = render_screen
            .depths
            .iter()
            .flat_map(|depth| depth.visuals.iter())
            .map(|visual| (visual.visual, visual.format))
            .collect();
        let root_format = visual_formats
            .iter()
            .find_map(|(visual, format)| (*visual == screen.root_visual).then_some(*format))
            .ok_or_else(|| invalid_data("XRender has no format for the root visual"))?;
        let root_visual = find_visual(screen, screen.root_visual)
            .ok_or_else(|| invalid_data("X11 root visual is not described by the screen"))?;
        if root_visual.class != VisualClass::TRUE_COLOR {
            return Err(
                invalid_data("remote capture requires an X11 TrueColor root visual").into(),
            );
        }
        let root_pixmap_format = conn
            .setup()
            .pixmap_formats
            .iter()
            .find(|format| format.depth == screen.root_depth)
            .copied()
            .ok_or_else(|| {
                invalid_data(format!(
                    "X11 has no pixmap format for root depth {}",
                    screen.root_depth
                ))
            })?;
        shm_buffer_size(1, 1, root_pixmap_format)?;

        Ok(Self {
            visual_formats,
            root: screen.root,
            root_depth: screen.root_depth,
            root_format,
            root_pixmap_format,
            root_layout: PixelLayout::from_visual_type(root_visual)?,
            root_standard_bgrx_visual: is_standard_bgrx_visual(root_visual),
            source: None,
            target: None,
            reported_dimensions: None,
        })
    }

    fn can_capture(
        &self,
        source: RenderCaptureSource,
        source_width: u16,
        source_height: u16,
    ) -> bool {
        match source {
            RenderCaptureSource::Overlay(_) => true,
            RenderCaptureSource::RootSnapshot => {
                root_staging_bytes(source_width, source_height, self.root_pixmap_format).is_some()
            }
        }
    }

    fn capture(
        &mut self,
        conn: &RustConnection,
        readback: &mut ShmReadback,
        capture_source: RenderCaptureSource,
        source_width: u16,
        source_height: u16,
        output_width: u16,
        output_height: u16,
    ) -> RemoteResult<RgbImage> {
        self.ensure_target(conn, output_width, output_height)?;
        self.ensure_source(conn, capture_source, source_width, source_height)?;
        let dimensions = (source_width, source_height, output_width, output_height);
        let source = self
            .source
            .as_mut()
            .ok_or_else(|| invalid_data("XRender source picture was not created"))?;
        if source.transform != Some(dimensions) {
            render::set_picture_transform(
                conn,
                source.picture,
                scale_transform(source_width, source_height, output_width, output_height)?,
            )?
            .check()?;
            source.transform = Some(dimensions);
        }
        let copy_source = match source.backing {
            RenderSourceBacking::Overlay { .. } => None,
            RenderSourceBacking::RootSnapshot { pixmap, gc, .. } => Some((pixmap, gc)),
        };
        let source_picture = source.picture;
        let target = self
            .target
            .as_ref()
            .ok_or_else(|| invalid_data("XRender target picture was not created"))?;
        let copy = match copy_source {
            Some((pixmap, gc)) => Some(conn.copy_area(
                self.root,
                pixmap,
                gc,
                0,
                0,
                0,
                0,
                source_width,
                source_height,
            )?),
            None => None,
        };
        let composite = match render::composite(
            conn,
            PictOp::SRC,
            source_picture,
            x11rb::NONE,
            target.picture,
            0,
            0,
            0,
            0,
            0,
            0,
            output_width,
            output_height,
        ) {
            Ok(cookie) => cookie,
            Err(error) => {
                if let Some(cookie) = copy {
                    let _ = cookie.check();
                }
                return Err(error.into());
            }
        };
        let image = readback.capture_rgb(
            conn,
            target.pixmap,
            output_width,
            output_height,
            ReadbackLayout::Known {
                expected_depth: self.root_depth,
                layout: self.root_layout,
                standard_bgrx_visual: self.root_standard_bgrx_visual,
            },
        );
        // The small ShmGetImage/core GetImage reply is an ordering barrier for
        // both queued requests. Consume both VoidCookies afterwards: precise
        // CopyArea/Render errors are retained without a steady-state RTT, and
        // a stale target is never published if either request was rejected.
        let copy_result: RemoteResult<()> = copy
            .map(|cookie| cookie.check().map_err(Into::into))
            .unwrap_or(Ok(()));
        let composite_result = composite.check().map_err(Into::into);
        let image = resolve_render_readback(image, copy_result, composite_result)?;
        if self.reported_dimensions != Some(dimensions) {
            eprintln!(
                "jwm-remote: XRender downscale {}x{} -> {}x{}",
                source_width, source_height, output_width, output_height
            );
            self.reported_dimensions = Some(dimensions);
        }
        Ok(image)
    }

    fn ensure_source(
        &mut self,
        conn: &RustConnection,
        source: RenderCaptureSource,
        width: u16,
        height: u16,
    ) -> RemoteResult<()> {
        match source {
            RenderCaptureSource::Overlay(drawable) => self.ensure_overlay_source(conn, drawable),
            RenderCaptureSource::RootSnapshot => self.ensure_root_snapshot(conn, width, height),
        }
    }

    fn ensure_overlay_source(
        &mut self,
        conn: &RustConnection,
        drawable: Window,
    ) -> RemoteResult<()> {
        if drawable == self.root {
            return Err(invalid_data(
                "the X11 root window must be copied into a staging pixmap before XRender",
            )
            .into());
        }
        if self.source.as_ref().is_some_and(|source| {
            matches!(
                source.backing,
                RenderSourceBacking::Overlay {
                    drawable: current
                } if current == drawable
            )
        }) {
            return Ok(());
        }
        let visual = conn.get_window_attributes(drawable)?.reply()?.visual;
        let format = self
            .visual_formats
            .iter()
            .find_map(|(candidate, format)| (*candidate == visual).then_some(*format))
            .ok_or_else(|| invalid_data("XRender has no format for the capture visual"))?;
        let picture = conn.generate_id()?;
        let create = render::create_picture(
            conn,
            picture,
            drawable,
            format,
            &CreatePictureAux::new().repeat(Repeat::PAD),
        )?
        .check();
        if let Err(error) = create {
            return Err(error.into());
        }
        let filter = match render::set_picture_filter(conn, picture, b"bilinear", &[]) {
            Ok(cookie) => cookie,
            Err(error) => {
                free_picture_checked(conn, picture);
                return Err(error.into());
            }
        };
        if let Err(error) = filter.check() {
            free_picture_checked(conn, picture);
            return Err(error.into());
        }

        let old = self.source.replace(RenderSource {
            backing: RenderSourceBacking::Overlay { drawable },
            picture,
            transform: None,
        });
        if let Some(old) = old {
            release_render_source(conn, old);
        }
        Ok(())
    }

    fn ensure_root_snapshot(
        &mut self,
        conn: &RustConnection,
        width: u16,
        height: u16,
    ) -> RemoteResult<()> {
        if self.source.as_ref().is_some_and(|source| {
            matches!(
                source.backing,
                RenderSourceBacking::RootSnapshot {
                    width: current_width,
                    height: current_height,
                    ..
                } if (current_width, current_height) == (width, height)
            )
        }) {
            return Ok(());
        }
        let bytes = root_staging_bytes(width, height, self.root_pixmap_format)
            .ok_or_else(|| invalid_data("XRender root staging pixmap exceeds the 64 MiB limit"))?;

        // Preserve the old usable source transactionally whenever doing so
        // stays inside the hard staging budget. A resize whose two pixmaps
        // would exceed 64 MiB first releases the obsolete snapshot; failure
        // then cleanly takes the existing same-frame CPU fallback.
        let old_root_bytes = self
            .source
            .as_ref()
            .and_then(|source| match source.backing {
                RenderSourceBacking::RootSnapshot { bytes, .. } => Some(bytes),
                RenderSourceBacking::Overlay { .. } => None,
            });
        if root_staging_replacement(old_root_bytes, bytes) == RootStagingReplacement::ReleaseOld
            && let Some(old) = self.source.take()
        {
            release_render_source(conn, old);
        }

        let pixmap = conn.generate_id()?;
        let gc = conn.generate_id()?;
        let picture = conn.generate_id()?;
        conn.create_pixmap(self.root_depth, pixmap, self.root, width, height)?
            .check()?;
        let create_gc = match conn.create_gc(
            gc,
            pixmap,
            &CreateGCAux::new()
                .subwindow_mode(SubwindowMode::INCLUDE_INFERIORS)
                .graphics_exposures(0_u32),
        ) {
            Ok(cookie) => cookie,
            Err(error) => {
                free_pixmap_checked(conn, pixmap);
                return Err(error.into());
            }
        };
        if let Err(error) = create_gc.check() {
            free_pixmap_checked(conn, pixmap);
            return Err(error.into());
        }
        let create_picture = match render::create_picture(
            conn,
            picture,
            pixmap,
            self.root_format,
            &CreatePictureAux::new().repeat(Repeat::PAD),
        ) {
            Ok(cookie) => cookie,
            Err(error) => {
                free_gc_and_pixmap_checked(conn, gc, pixmap);
                return Err(error.into());
            }
        };
        if let Err(error) = create_picture.check() {
            free_gc_and_pixmap_checked(conn, gc, pixmap);
            return Err(error.into());
        }
        let filter = match render::set_picture_filter(conn, picture, b"bilinear", &[]) {
            Ok(cookie) => cookie,
            Err(error) => {
                release_root_snapshot_ids(conn, picture, gc, pixmap);
                return Err(error.into());
            }
        };
        if let Err(error) = filter.check() {
            release_root_snapshot_ids(conn, picture, gc, pixmap);
            return Err(error.into());
        }

        let old = self.source.replace(RenderSource {
            backing: RenderSourceBacking::RootSnapshot {
                pixmap,
                gc,
                width,
                height,
                bytes,
            },
            picture,
            transform: None,
        });
        if let Some(old) = old {
            release_render_source(conn, old);
        }
        Ok(())
    }

    fn ensure_target(
        &mut self,
        conn: &RustConnection,
        width: u16,
        height: u16,
    ) -> RemoteResult<()> {
        if self
            .target
            .as_ref()
            .is_some_and(|target| target.width == width && target.height == height)
        {
            return Ok(());
        }

        let pixmap = conn.generate_id()?;
        let picture = conn.generate_id()?;
        conn.create_pixmap(self.root_depth, pixmap, self.root, width, height)?
            .check()?;
        let create = match render::create_picture(
            conn,
            picture,
            pixmap,
            self.root_format,
            &CreatePictureAux::new(),
        ) {
            Ok(cookie) => cookie.check(),
            Err(error) => {
                free_pixmap_checked(conn, pixmap);
                return Err(error.into());
            }
        };
        if let Err(error) = create {
            free_pixmap_checked(conn, pixmap);
            return Err(error.into());
        }

        let old = self.target.replace(RenderTarget {
            pixmap,
            picture,
            width,
            height,
        });
        if let Some(old) = old {
            release_render_target(conn, old);
        }
        Ok(())
    }

    fn release_source(&mut self, conn: &RustConnection) {
        if let Some(source) = self.source.take() {
            release_render_source(conn, source);
        }
    }

    fn release(&mut self, conn: &RustConnection) {
        self.release_source(conn);
        if let Some(target) = self.target.take() {
            release_render_target(conn, target);
        }
    }
}

fn release_render_source(conn: &RustConnection, source: RenderSource) {
    match source.backing {
        RenderSourceBacking::Overlay { .. } => free_picture_checked(conn, source.picture),
        RenderSourceBacking::RootSnapshot { pixmap, gc, .. } => {
            release_root_snapshot_ids(conn, source.picture, gc, pixmap);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RootSnapshotResource {
    Picture(Picture),
    Gc(Gcontext),
    Pixmap(Pixmap),
}

fn root_snapshot_release_plan(
    picture: Option<Picture>,
    gc: Option<Gcontext>,
    pixmap: Option<Pixmap>,
) -> [Option<RootSnapshotResource>; 3] {
    [
        picture.map(RootSnapshotResource::Picture),
        gc.map(RootSnapshotResource::Gc),
        pixmap.map(RootSnapshotResource::Pixmap),
    ]
}

fn release_root_snapshot_resources(
    conn: &RustConnection,
    resources: [Option<RootSnapshotResource>; 3],
) {
    let cookies = resources.map(|resource| {
        resource.and_then(|resource| match resource {
            RootSnapshotResource::Picture(picture) => render::free_picture(conn, picture).ok(),
            RootSnapshotResource::Gc(gc) => conn.free_gc(gc).ok(),
            RootSnapshotResource::Pixmap(pixmap) => conn.free_pixmap(pixmap).ok(),
        })
    });
    for cookie in cookies.into_iter().flatten() {
        let _ = cookie.check();
    }
}

fn release_root_snapshot_ids(
    conn: &RustConnection,
    picture: Picture,
    gc: Gcontext,
    pixmap: Pixmap,
) {
    // Queue the reverse-order destruction first. Checking the earliest cookie
    // then uses the later queued requests as part of the same synchronization,
    // and every cookie is consumed before the next event drain.
    release_root_snapshot_resources(
        conn,
        root_snapshot_release_plan(Some(picture), Some(gc), Some(pixmap)),
    );
}

fn release_render_target(conn: &RustConnection, target: RenderTarget) {
    let picture_cookie = render::free_picture(conn, target.picture).ok();
    let pixmap_cookie = conn.free_pixmap(target.pixmap).ok();
    if let Some(cookie) = picture_cookie {
        let _ = cookie.check();
    }
    if let Some(cookie) = pixmap_cookie {
        let _ = cookie.check();
    }
}

fn free_picture_checked(conn: &RustConnection, picture: Picture) {
    if let Ok(cookie) = render::free_picture(conn, picture) {
        let _ = cookie.check();
    }
}

fn free_pixmap_checked(conn: &RustConnection, pixmap: Pixmap) {
    if let Ok(cookie) = conn.free_pixmap(pixmap) {
        let _ = cookie.check();
    }
}

fn free_gc_and_pixmap_checked(conn: &RustConnection, gc: Gcontext, pixmap: Pixmap) {
    release_root_snapshot_resources(
        conn,
        root_snapshot_release_plan(None, Some(gc), Some(pixmap)),
    );
}

fn scale_transform(
    source_width: u16,
    source_height: u16,
    output_width: u16,
    output_height: u16,
) -> RemoteResult<Transform> {
    fn fixed_ratio(source: u16, output: u16) -> RemoteResult<i32> {
        if output == 0 {
            return Err(invalid_data("XRender output dimension is zero").into());
        }
        let denominator = i64::from(output);
        let fixed = ((i64::from(source) << 16) + denominator / 2) / denominator;
        i32::try_from(fixed)
            .map_err(|_| invalid_data("XRender scale transform exceeds 16.16 range").into())
    }

    Ok(Transform {
        matrix11: fixed_ratio(source_width, output_width)?,
        matrix12: 0,
        matrix13: 0,
        matrix21: 0,
        matrix22: fixed_ratio(source_height, output_height)?,
        matrix23: 0,
        matrix31: 0,
        matrix32: 0,
        matrix33: 1 << 16,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReadbackDecoder {
    Bgrx32,
    Generic,
}

fn is_standard_bgrx_visual(visual: Visualtype) -> bool {
    visual.class == VisualClass::TRUE_COLOR
        && visual.bits_per_rgb_value == 8
        && visual.red_mask == 0x00ff_0000
        && visual.green_mask == 0x0000_ff00
        && visual.blue_mask == 0x0000_00ff
}

fn select_readback_decoder(ximage: &XImage<'_>, standard_bgrx_visual: bool) -> ReadbackDecoder {
    if standard_bgrx_visual
        && ximage.depth() == 24
        && ximage.bits_per_pixel() == BitsPerPixel::B32
        && ximage.scanline_pad() == ScanlinePad::Pad32
        && ximage.byte_order() == ImageOrder::LsbFirst
    {
        ReadbackDecoder::Bgrx32
    } else {
        ReadbackDecoder::Generic
    }
}

fn decode_ximage(
    ximage: &XImage<'_>,
    width: u16,
    height: u16,
    layout: PixelLayout,
    standard_bgrx_visual: bool,
) -> RemoteResult<RgbImage> {
    if width == 0 || height == 0 {
        return Err(invalid_data("X11 capture dimensions must be nonzero").into());
    }
    if (ximage.width(), ximage.height()) != (width, height) {
        return Err(invalid_data(format!(
            "X11 capture image geometry changed from {width}x{height} to {}x{}",
            ximage.width(),
            ximage.height()
        ))
        .into());
    }
    if select_readback_decoder(ximage, standard_bgrx_visual) == ReadbackDecoder::Bgrx32 {
        let stride = native_scanline_stride(width, ximage.bits_per_pixel(), ximage.scanline_pad())?;
        return decode_bgrx32_rows(ximage.data(), width, height, stride);
    }
    decode_ximage_generic(ximage, width, height, layout)
}

fn native_scanline_stride(
    width: u16,
    bits_per_pixel: BitsPerPixel,
    scanline_pad: ScanlinePad,
) -> RemoteResult<usize> {
    let bits_per_pixel = usize::from(bits_per_pixel);
    let scanline_pad = usize::from(scanline_pad);
    let row_bits = usize::from(width)
        .checked_mul(bits_per_pixel)
        .ok_or_else(|| invalid_data("X11 capture scanline size overflow"))?;
    let padded_units = row_bits
        .checked_add(scanline_pad - 1)
        .ok_or_else(|| invalid_data("X11 capture scanline padding overflow"))?
        / scanline_pad;
    let padded_bits = padded_units
        .checked_mul(scanline_pad)
        .ok_or_else(|| invalid_data("X11 capture padded scanline size overflow"))?;
    Ok(padded_bits / 8)
}

fn decode_bgrx32_rows(
    source: &[u8],
    width: u16,
    height: u16,
    stride: usize,
) -> RemoteResult<RgbImage> {
    if width == 0 || height == 0 {
        return Err(invalid_data("X11 capture dimensions must be nonzero").into());
    }
    let source_row_bytes = usize::from(width)
        .checked_mul(4)
        .ok_or_else(|| invalid_data("X11 BGRX scanline size overflow"))?;
    if stride < source_row_bytes {
        return Err(invalid_data(format!(
            "X11 BGRX stride is too short: {stride} < {source_row_bytes}"
        ))
        .into());
    }
    let source_bytes = stride
        .checked_mul(usize::from(height))
        .ok_or_else(|| invalid_data("X11 BGRX image size overflow"))?;
    if source.len() < source_bytes {
        return Err(invalid_data(format!(
            "X11 BGRX image is truncated: {} < {source_bytes}",
            source.len()
        ))
        .into());
    }
    let rgb_row_bytes = usize::from(width)
        .checked_mul(3)
        .ok_or_else(|| invalid_data("X11 RGB scanline size overflow"))?;
    let rgb_bytes = rgb_row_bytes
        .checked_mul(usize::from(height))
        .ok_or_else(|| invalid_data("X11 capture buffer size overflow"))?;
    let mut rgb = Vec::new();
    rgb.try_reserve_exact(rgb_bytes)
        .map_err(|_| invalid_data("could not allocate the X11 capture buffer"))?;
    rgb.resize(rgb_bytes, 0);

    for row in 0..usize::from(height) {
        let source_start = row * stride;
        let source_row = &source[source_start..source_start + source_row_bytes];
        let rgb_start = row * rgb_row_bytes;
        let rgb_row = &mut rgb[rgb_start..rgb_start + rgb_row_bytes];
        for (pixel, output) in source_row.chunks_exact(4).zip(rgb_row.chunks_exact_mut(3)) {
            output.copy_from_slice(&[pixel[2], pixel[1], pixel[0]]);
        }
    }

    RgbImage::from_raw(u32::from(width), u32::from(height), rgb)
        .ok_or_else(|| invalid_data("X11 capture returned an invalid pixel buffer").into())
}

fn decode_ximage_generic(
    ximage: &XImage<'_>,
    width: u16,
    height: u16,
    layout: PixelLayout,
) -> RemoteResult<RgbImage> {
    let pixel_count = usize::from(width)
        .checked_mul(usize::from(height))
        .ok_or_else(|| invalid_data("X11 capture dimensions overflow"))?;
    let rgb_bytes = pixel_count
        .checked_mul(3)
        .ok_or_else(|| invalid_data("X11 capture buffer size overflow"))?;
    let mut rgb = Vec::new();
    rgb.try_reserve_exact(rgb_bytes)
        .map_err(|_| invalid_data("could not allocate the X11 capture buffer"))?;
    for y in 0..height {
        for x in 0..width {
            let (red, green, blue) = layout.decode(ximage.get_pixel(x, y));
            rgb.extend_from_slice(&[(red >> 8) as u8, (green >> 8) as u8, (blue >> 8) as u8]);
        }
    }
    RgbImage::from_raw(u32::from(width), u32::from(height), rgb)
        .ok_or_else(|| invalid_data("X11 capture returned an invalid pixel buffer").into())
}

fn acquire_overlay(conn: &RustConnection, root: Window) -> RemoteResult<Window> {
    Ok(composite::get_overlay_window(conn, root)?
        .reply()?
        .overlay_win)
}

fn compositor_selection(conn: &RustConnection, screen_num: usize) -> RemoteResult<Atom> {
    let selection = format!("_NET_WM_CM_S{screen_num}");
    Ok(conn.intern_atom(false, selection.as_bytes())?.reply()?.atom)
}

fn query_composite_overlay(conn: &RustConnection) -> RemoteResult<()> {
    let version =
        composite::query_version(conn, COMPOSITE_CLIENT_VERSION.0, COMPOSITE_CLIENT_VERSION.1)?
            .reply()?;
    if (version.major_version, version.minor_version) < COMPOSITE_OVERLAY_VERSION {
        return Err(invalid_data(format!(
            "X Composite {}.{} is too old; overlay capture requires {}.{}",
            version.major_version,
            version.minor_version,
            COMPOSITE_OVERLAY_VERSION.0,
            COMPOSITE_OVERLAY_VERSION.1
        ))
        .into());
    }
    Ok(())
}

fn query_xfixes(conn: &RustConnection) -> RemoteResult<()> {
    let version =
        xfixes::query_version(conn, XFIXES_CLIENT_VERSION.0, XFIXES_CLIENT_VERSION.1)?.reply()?;
    if !supports_xfixes_capture(version.major_version, version.minor_version) {
        return Err(invalid_data(format!(
            "XFixes {}.{} is too old; cursor and selection notifications require 1.0",
            version.major_version, version.minor_version
        ))
        .into());
    }
    Ok(())
}

fn supports_xfixes_capture(major: u32, minor: u32) -> bool {
    (major, minor) >= (1, 0)
}

fn select_cursor_events(conn: &RustConnection, root: Window) -> bool {
    let result: RemoteResult<()> = (|| {
        xfixes::select_cursor_input(conn, root, xfixes::CursorNotifyMask::DISPLAY_CURSOR)?
            .check()?;
        Ok(())
    })();
    match result {
        Ok(()) => true,
        Err(error) => {
            eprintln!(
                "remote: XFixes cursor notifications unavailable ({error}); polling cursor images"
            );
            false
        }
    }
}

fn select_compositor_events(conn: &RustConnection, root: Window, selection: Atom) -> bool {
    let mask = xfixes::SelectionEventMask::SET_SELECTION_OWNER
        | xfixes::SelectionEventMask::SELECTION_WINDOW_DESTROY
        | xfixes::SelectionEventMask::SELECTION_CLIENT_CLOSE;
    let result: RemoteResult<()> = (|| {
        xfixes::select_selection_input(conn, root, selection, mask)?.check()?;
        Ok(())
    })();
    match result {
        Ok(()) => true,
        Err(error) => {
            eprintln!(
                "remote: XFixes compositor-owner notifications unavailable ({error}); polling the owner"
            );
            false
        }
    }
}

fn select_geometry_events(conn: &RustConnection, root: Window) -> bool {
    let core_result: RemoteResult<()> = (|| {
        let attributes = conn.get_window_attributes(root)?.reply()?;
        let mask = attributes.your_event_mask | EventMask::STRUCTURE_NOTIFY;
        conn.change_window_attributes(root, &ChangeWindowAttributesAux::new().event_mask(mask))?
            .check()?;
        Ok(())
    })();

    let randr_result: RemoteResult<bool> = (|| {
        if conn
            .extension_information(randr::X11_EXTENSION_NAME)?
            .is_none()
        {
            return Ok(false);
        }
        // RandR requires QueryVersion before every other extension request.
        let version =
            randr::query_version(conn, RANDR_CLIENT_VERSION.0, RANDR_CLIENT_VERSION.1)?.reply()?;
        if version.major_version < 1 {
            return Err(invalid_data(format!(
                "RandR {}.{} lacks screen-change notifications",
                version.major_version, version.minor_version
            ))
            .into());
        }
        randr::select_input(conn, root, randr::NotifyMask::SCREEN_CHANGE)?.check()?;
        Ok(true)
    })();

    let core_ready = core_result.is_ok();
    let randr_ready = matches!(randr_result, Ok(true));
    if !core_ready && !randr_ready {
        let core_error = core_result
            .err()
            .map_or_else(|| "unavailable".to_owned(), |error| error.to_string());
        let randr_error = match randr_result {
            Ok(false) => "extension unavailable".to_owned(),
            Ok(true) => unreachable!(),
            Err(error) => error.to_string(),
        };
        eprintln!(
            "remote: root geometry notifications unavailable (core: {core_error}; RandR: {randr_error}); polling geometry"
        );
    }
    core_ready || randr_ready
}

fn install_capture_inhibitor(conn: &RustConnection, root: Window) -> RemoteResult<(Atom, Window)> {
    let atom = conn.intern_atom(false, REMOTE_CAPTURE_OWNER)?.reply()?.atom;
    let owner = conn.generate_id()?;
    conn.grab_server()?.check()?;
    let mut created = false;
    let result = (|| -> RemoteResult<()> {
        let existing = conn
            .get_property(false, root, atom, AtomEnum::WINDOW, 0, 1)?
            .reply()?
            .value32()
            .and_then(|mut values| values.next())
            .filter(|owner| *owner != x11rb::NONE);
        if let Some(existing) = existing {
            let live = match conn.get_window_attributes(existing) {
                Ok(cookie) => cookie.reply().is_ok(),
                Err(error) => return Err(error.into()),
            };
            if live {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "another jwm-remote host already captures this X11 screen",
                )
                .into());
            }
        }
        conn.create_window(
            0,
            owner,
            root,
            -1,
            -1,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            0,
            &CreateWindowAux::new(),
        )?
        .check()?;
        created = true;
        conn.change_property32(PropMode::REPLACE, root, atom, AtomEnum::WINDOW, &[owner])?
            .check()?;
        conn.flush()?;
        Ok(())
    })();
    let ungrab_result: RemoteResult<()> = (|| {
        conn.ungrab_server()?.check()?;
        conn.flush()?;
        Ok(())
    })();
    if let Err(error) = result {
        if created {
            let _ = conn.destroy_window(owner);
        }
        let _ = conn.flush();
        return Err(error);
    }
    if let Err(error) = ungrab_result {
        let _ = conn.destroy_window(owner);
        let _ = conn.flush();
        return Err(error);
    }
    Ok((atom, owner))
}

fn scale_length(value: u16, output: u32, source: u16) -> u32 {
    ((u64::from(value) * u64::from(output) + u64::from(source) / 2) / u64::from(source)).max(1)
        as u32
}

fn scale_coordinate(value: i32, output: u32, source: u16) -> i32 {
    let numerator = i64::from(value) * i64::from(output);
    let half = i64::from(source) / 2;
    let rounded = if numerator >= 0 {
        (numerator + half) / i64::from(source)
    } else {
        (numerator - half) / i64::from(source)
    };
    rounded.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

impl Drop for X11Capture {
    fn drop(&mut self) {
        self.release_overlay_cleanup();
        self.release_render_scaler();
        self.shm_readback.release(&self.conn);
        if let Ok(cookie) = self.conn.get_property(
            false,
            self.root,
            self.inhibitor_atom,
            AtomEnum::WINDOW,
            0,
            1,
        ) && let Ok(reply) = cookie.reply()
            && reply.value32().and_then(|mut values| values.next()) == Some(self.inhibitor_window)
        {
            if let Ok(cookie) = self.conn.delete_property(self.root, self.inhibitor_atom) {
                let _ = cookie.check();
            }
        }
        if let Ok(cookie) = self.conn.destroy_window(self.inhibitor_window) {
            let _ = cookie.check();
        }
        let _ = self.conn.sync();
    }
}

fn find_visual(screen: &Screen, visual_id: u32) -> Option<Visualtype> {
    screen
        .allowed_depths
        .iter()
        .flat_map(|depth| depth.visuals.iter())
        .find(|visual| visual.visual_id == visual_id)
        .copied()
}

#[must_use]
pub fn scaled_dimensions(width: u16, height: u16, max_width: u16) -> (u16, u16) {
    if max_width == 0 || width <= max_width || height == 0 {
        return (width, height);
    }
    let scaled_height = (u32::from(height) * u32::from(max_width) / u32::from(width))
        .clamp(1, u32::from(u16::MAX)) as u16;
    (max_width, scaled_height)
}

fn validate_root_geometry(width: u16, height: u16) -> RemoteResult<()> {
    if width == 0 || height == 0 {
        return Err(invalid_data("X11 root has an empty geometry").into());
    }
    super::frame::validate_dimensions(width, height)
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn standard_bgrx_visual() -> Visualtype {
        Visualtype {
            visual_id: 7,
            class: VisualClass::TRUE_COLOR,
            bits_per_rgb_value: 8,
            colormap_entries: 256,
            red_mask: 0x00ff_0000,
            green_mask: 0x0000_ff00,
            blue_mask: 0x0000_00ff,
        }
    }

    #[test]
    fn downscale_preserves_aspect_ratio_without_upscaling() {
        assert_eq!(scaled_dimensions(1920, 1080, 1280), (1280, 720));
        assert_eq!(scaled_dimensions(2560, 1440, 1920), (1920, 1080));
        assert_eq!(scaled_dimensions(1024, 768, 1280), (1024, 768));
        assert_eq!(scaled_dimensions(1024, 768, 0), (1024, 768));
    }

    #[test]
    fn tiny_aspect_ratios_keep_a_nonzero_height() {
        assert_eq!(scaled_dimensions(u16::MAX, 1, 1), (1, 1));
    }

    #[test]
    fn xrender_transform_maps_destination_back_to_source() {
        let transform = scale_transform(1920, 1080, 1280, 720).unwrap();
        assert_eq!(transform.matrix11, 98_304);
        assert_eq!(transform.matrix22, 98_304);
        assert_eq!(transform.matrix33, 65_536);
        assert_eq!(transform.matrix12, 0);
        assert_eq!(transform.matrix21, 0);
    }

    #[test]
    fn root_and_overlay_have_distinct_xrender_source_types() {
        let root = 41;
        assert_eq!(
            render_capture_source(root, root),
            RenderCaptureSource::RootSnapshot
        );
        assert_eq!(
            render_capture_source(root, 42),
            RenderCaptureSource::Overlay(42)
        );
    }

    #[test]
    fn root_staging_native_size_has_a_hard_64_mib_limit() {
        let depth_24_in_32 = Format {
            depth: 24,
            bits_per_pixel: 32,
            scanline_pad: 32,
        };
        assert_eq!(
            root_staging_bytes(4096, 4096, depth_24_in_32),
            Some(ROOT_STAGING_MAX_BYTES)
        );
        assert_eq!(root_staging_bytes(4097, 4096, depth_24_in_32), None);
        assert_eq!(
            root_staging_bytes(u16::MAX, u16::MAX, depth_24_in_32),
            None,
            "protocol overflow is also ineligible instead of allocating"
        );

        let padded_16 = Format {
            depth: 16,
            bits_per_pixel: 16,
            scanline_pad: 32,
        };
        assert_eq!(root_staging_bytes(3, 2, padded_16), Some(16));
    }

    #[test]
    fn root_staging_replacement_never_exceeds_its_total_budget() {
        assert_eq!(
            root_staging_replacement(None, ROOT_STAGING_MAX_BYTES),
            RootStagingReplacement::PreserveOld
        );
        assert_eq!(
            root_staging_replacement(Some(ROOT_STAGING_MAX_BYTES / 2), ROOT_STAGING_MAX_BYTES / 2),
            RootStagingReplacement::PreserveOld
        );
        assert_eq!(
            root_staging_replacement(Some(1), ROOT_STAGING_MAX_BYTES),
            RootStagingReplacement::ReleaseOld
        );
        assert_eq!(
            root_staging_replacement(Some(usize::MAX), 1),
            RootStagingReplacement::ReleaseOld,
            "overflow cannot bypass the cap"
        );
    }

    #[test]
    fn render_request_errors_never_publish_the_small_readback() {
        assert_eq!(
            resolve_render_readback(Ok::<_, &str>(7), Ok(()), Ok(())),
            Ok(7)
        );
        assert_eq!(
            resolve_render_readback(Ok(7), Err("copy"), Err("composite")),
            Err("copy")
        );
        assert_eq!(
            resolve_render_readback(Ok(7), Ok(()), Err("composite")),
            Err("composite")
        );
        assert_eq!(
            resolve_render_readback(Err::<u8, _>("readback"), Ok(()), Ok(())),
            Err("readback")
        );
    }

    #[test]
    fn root_snapshot_cleanup_is_reverse_order_and_consumed_once() {
        assert_eq!(
            root_snapshot_release_plan(Some(1), Some(2), Some(3)),
            [
                Some(RootSnapshotResource::Picture(1)),
                Some(RootSnapshotResource::Gc(2)),
                Some(RootSnapshotResource::Pixmap(3)),
            ]
        );
        assert_eq!(
            root_snapshot_release_plan(None, Some(2), Some(3)),
            [
                None,
                Some(RootSnapshotResource::Gc(2)),
                Some(RootSnapshotResource::Pixmap(3)),
            ]
        );
        assert_eq!(
            root_snapshot_release_plan(None, None, Some(3)),
            [None, None, Some(RootSnapshotResource::Pixmap(3))]
        );
        assert_eq!(root_snapshot_release_plan(None, None, None), [None; 3]);
    }

    #[test]
    fn root_geometry_epoch_retries_once_then_uses_cpu() {
        let mut geometry = RootGeometryCache::new(1920, 1080, true);
        let captured_epoch = geometry.epoch();
        assert!(geometry.event_snapshot_is_current(captured_epoch));

        // A same-WxH ConfigureNotify or RandR epoch is still a new desktop
        // topology and must invalidate the staged image.
        geometry.invalidate();
        assert_eq!((geometry.width, geometry.height), (1920, 1080));
        assert!(!geometry.event_snapshot_is_current(captured_epoch));
        assert_eq!(
            root_snapshot_race_action(true, false),
            RootSnapshotRaceAction::Retry
        );
        assert_eq!(
            root_snapshot_race_action(false, false),
            RootSnapshotRaceAction::CpuFallback
        );
        assert_eq!(
            root_snapshot_race_action(false, true),
            RootSnapshotRaceAction::Retry,
            "a root-to-overlay transition gets the newly authoritative source"
        );

        assert!(geometry.fall_back_to_polling());
        assert!(geometry.needs_post_capture_query());
    }

    #[test]
    fn bgrx_readback_selector_requires_the_exact_native_format_and_visual() {
        let image = |depth, bits_per_pixel, scanline_pad, byte_order, bytes: usize| {
            XImage::new(
                1,
                1,
                scanline_pad,
                depth,
                bits_per_pixel,
                byte_order,
                Cow::Owned(vec![0; bytes]),
            )
            .unwrap()
        };
        let native = image(
            24,
            BitsPerPixel::B32,
            ScanlinePad::Pad32,
            ImageOrder::LsbFirst,
            4,
        );
        assert!(is_standard_bgrx_visual(standard_bgrx_visual()));
        assert_eq!(
            select_readback_decoder(&native, true),
            ReadbackDecoder::Bgrx32
        );
        assert_eq!(
            select_readback_decoder(&native, false),
            ReadbackDecoder::Generic
        );

        let mut nonstandard = standard_bgrx_visual();
        nonstandard.red_mask = 0x0000_00ff;
        nonstandard.blue_mask = 0x00ff_0000;
        assert!(!is_standard_bgrx_visual(nonstandard));
        let mut direct_color = standard_bgrx_visual();
        direct_color.class = VisualClass::DIRECT_COLOR;
        assert!(!is_standard_bgrx_visual(direct_color));

        for ineligible in [
            image(
                32,
                BitsPerPixel::B32,
                ScanlinePad::Pad32,
                ImageOrder::LsbFirst,
                4,
            ),
            image(
                24,
                BitsPerPixel::B32,
                ScanlinePad::Pad16,
                ImageOrder::LsbFirst,
                4,
            ),
            image(
                24,
                BitsPerPixel::B32,
                ScanlinePad::Pad32,
                ImageOrder::MsbFirst,
                4,
            ),
            image(
                24,
                BitsPerPixel::B24,
                ScanlinePad::Pad32,
                ImageOrder::LsbFirst,
                4,
            ),
            image(
                16,
                BitsPerPixel::B16,
                ScanlinePad::Pad32,
                ImageOrder::LsbFirst,
                4,
            ),
        ] {
            assert_eq!(
                select_readback_decoder(&ineligible, true),
                ReadbackDecoder::Generic
            );
        }
    }

    #[test]
    fn bgrx_readback_matches_generic_decoder_for_random_odd_sized_images() {
        let layout = PixelLayout::from_visual_type(standard_bgrx_visual()).unwrap();
        let mut state = 0x8bad_f00du32;
        for (width, height) in [(1, 1), (3, 5), (17, 4)] {
            let stride = usize::from(width) * 4;
            let mut bytes = vec![0; stride * usize::from(height)];
            for pixel in bytes.chunks_exact_mut(4) {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                pixel.copy_from_slice(&state.to_le_bytes());
                // The unused X byte is deliberately nonzero.
                pixel[3] |= 0x80;
            }
            let image = XImage::new(
                width,
                height,
                ScanlinePad::Pad32,
                24,
                BitsPerPixel::B32,
                ImageOrder::LsbFirst,
                Cow::Owned(bytes),
            )
            .unwrap();
            let fast = decode_ximage(&image, width, height, layout, true).unwrap();
            let generic = decode_ximage_generic(&image, width, height, layout).unwrap();
            assert_eq!(fast, generic, "{width}x{height}");
        }
    }

    #[test]
    fn bgrx_rows_ignore_explicit_scanline_padding() {
        let mut bytes = vec![0xee; 32];
        bytes[0..12].copy_from_slice(&[3, 2, 1, 0xa1, 6, 5, 4, 0xa2, 9, 8, 7, 0xa3]);
        bytes[16..28].copy_from_slice(&[12, 11, 10, 0xb1, 15, 14, 13, 0xb2, 18, 17, 16, 0xb3]);
        let image = decode_bgrx32_rows(&bytes, 3, 2, 16).unwrap();
        assert_eq!(
            image.into_raw(),
            vec![
                1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18
            ]
        );
    }

    #[test]
    fn nonstandard_visual_uses_generic_pixel_layout() {
        let mut visual = standard_bgrx_visual();
        visual.red_mask = 0x0000_00ff;
        visual.blue_mask = 0x00ff_0000;
        let layout = PixelLayout::from_visual_type(visual).unwrap();
        let image = XImage::new(
            1,
            1,
            ScanlinePad::Pad32,
            24,
            BitsPerPixel::B32,
            ImageOrder::LsbFirst,
            Cow::Owned(vec![0x11, 0x22, 0x33, 0x99]),
        )
        .unwrap();
        assert_eq!(
            decode_ximage(&image, 1, 1, layout, is_standard_bgrx_visual(visual))
                .unwrap()
                .into_raw(),
            vec![0x11, 0x22, 0x33]
        );
    }

    #[test]
    fn bgrx_readback_rejects_bad_dimensions_stride_and_lengths() {
        assert!(decode_bgrx32_rows(&[], 0, 1, 0).is_err());
        assert!(decode_bgrx32_rows(&[0; 24], 3, 2, 11).is_err());
        assert!(decode_bgrx32_rows(&[0; 23], 3, 2, 12).is_err());
        assert!(decode_bgrx32_rows(&[0; 16], 1, 2, usize::MAX).is_err());

        let image = XImage::new(
            2,
            1,
            ScanlinePad::Pad32,
            24,
            BitsPerPixel::B32,
            ImageOrder::LsbFirst,
            Cow::Owned(vec![0; 8]),
        )
        .unwrap();
        let layout = PixelLayout::from_visual_type(standard_bgrx_visual()).unwrap();
        assert!(decode_ximage(&image, 1, 1, layout, true).is_err());
    }

    #[test]
    fn capture_source_parser_is_strict_but_accepts_compositor_alias() {
        assert_eq!("auto".parse(), Ok(CaptureSource::Auto));
        assert_eq!("compositor".parse(), Ok(CaptureSource::Overlay));
        assert!("window".parse::<CaptureSource>().is_err());
    }

    fn clean_damage_gate(
        captured_at: Instant,
        geometry: (u16, u16),
        cursor: CursorSnapshot,
    ) -> DamageGateState {
        let mut gate = DamageGateState::new();
        gate.subtract_queued();
        gate.capture_succeeded(captured_at, geometry, cursor);
        assert!(!gate.dirty);
        gate
    }

    #[test]
    fn damage_gate_skips_stable_frames_without_advancing_force_deadline() {
        let base = Instant::now();
        let geometry = (1920, 1080);
        let cursor = CursorSnapshot::Position(Some((20, 30)));
        let mut gate = clean_damage_gate(base, geometry, cursor);

        assert_eq!(
            gate.decide(base + Duration::from_secs(1), geometry, cursor, false),
            DamageGateDecision::NoChange
        );
        assert_eq!(gate.last_capture, Some(base));
        assert_eq!(
            gate.decide(base + DAMAGE_FORCE_REFRESH, geometry, cursor, false),
            DamageGateDecision::Capture
        );
        // Merely deciding to capture (or a later mailbox/session failure) does
        // not postpone the forced-refresh deadline.
        assert_eq!(
            gate.decide(base + Duration::from_secs(3), geometry, cursor, false),
            DamageGateDecision::Capture
        );

        gate.subtract_queued();
        gate.capture_succeeded(base + Duration::from_secs(3), geometry, cursor);
        assert_eq!(
            gate.decide(base + Duration::from_secs(4), geometry, cursor, false),
            DamageGateDecision::NoChange
        );
    }

    #[test]
    fn dirty_animation_short_circuits_the_gate_probe_and_queries_only_for_capture() {
        let gate = DamageGateState::new();
        let gate_queries = Cell::new(0_u32);
        let decision = gate
            .decide_with_cursor_probe(Instant::now(), (1920, 1080), false, || {
                gate_queries.set(gate_queries.get() + 1);
                Ok::<_, ()>(Some(CursorSnapshot::Position(Some((1, 2)))))
            })
            .unwrap();
        assert_eq!(decision, DamageGateDecision::Capture);
        assert_eq!(
            gate_queries.get(),
            0,
            "Damage already made capture mandatory"
        );

        // capture_drawable performs the one authoritative QueryPointer whose
        // result is actually composited into the frame.
        let capture_queries = Cell::new(0_u32);
        if decision == DamageGateDecision::Capture {
            capture_queries.set(capture_queries.get() + 1);
        }
        assert_eq!(capture_queries.get(), 1);
    }

    #[test]
    fn only_a_clean_gate_probes_pointer_position() {
        let base = Instant::now();
        let geometry = (1280, 720);
        let cursor = CursorSnapshot::Position(Some((10, 20)));
        let gate = clean_damage_gate(base, geometry, cursor);
        let probes = Cell::new(0_u32);
        let stable = gate
            .decide_with_cursor_probe(base + Duration::from_secs(1), geometry, false, || {
                probes.set(probes.get() + 1);
                Ok::<_, ()>(Some(cursor))
            })
            .unwrap();
        assert_eq!(stable, DamageGateDecision::NoChange);
        assert_eq!(probes.get(), 1);

        for (now, candidate_geometry, shape_dirty) in [
            (base, (640, 480), false),
            (base, geometry, true),
            (base + DAMAGE_FORCE_REFRESH, geometry, false),
        ] {
            let decision = gate
                .decide_with_cursor_probe(now, candidate_geometry, shape_dirty, || {
                    probes.set(probes.get() + 1);
                    Ok::<_, ()>(Some(cursor))
                })
                .unwrap();
            assert_eq!(decision, DamageGateDecision::Capture);
        }
        assert_eq!(probes.get(), 1, "mandatory captures must not probe again");
    }

    #[test]
    fn cursor_probe_is_never_committed_in_place_of_the_captured_position() {
        let base = Instant::now();
        let geometry = (1024, 768);
        let actual_a = CursorSnapshot::Position(Some((10, 10)));
        let probe_b = CursorSnapshot::Position(Some((20, 20)));
        let mut gate = clean_damage_gate(base, geometry, actual_a);

        assert_eq!(
            gate.decide_with_cursor_probe(
                base + Duration::from_millis(1),
                geometry,
                false,
                || Ok::<_, ()>(Some(probe_b)),
            )
            .unwrap(),
            DamageGateDecision::Capture
        );
        gate.subtract_queued();
        // The cursor moved back while capture was beginning; this is the
        // position actually queried and composited by capture_drawable.
        gate.capture_succeeded(base + Duration::from_millis(2), geometry, actual_a);
        assert_eq!(gate.last_cursor, Some(actual_a));
        assert_eq!(
            gate.decide_with_cursor_probe(
                base + Duration::from_millis(3),
                geometry,
                false,
                || Ok::<_, ()>(Some(actual_a)),
            )
            .unwrap(),
            DamageGateDecision::NoChange
        );
    }

    #[test]
    fn cursor_probe_failures_preserve_recoverable_and_fatal_session_policies() {
        assert_eq!(
            cursor_probe_failure_action(CursorFailureKind::Recoverable),
            CursorProbeFailureAction::DisableAndCapture
        );
        assert_eq!(
            cursor_probe_failure_action(CursorFailureKind::Fatal),
            CursorProbeFailureAction::Fatal
        );

        let cursor = CursorCapture::disabled();
        assert!(!cursor.needs_shape());
        assert_eq!(
            captured_cursor_snapshot(&cursor, None),
            CursorSnapshot::Disabled,
            "the recoverable path publishes a full frame with cursor disabled"
        );
    }

    #[test]
    fn xfixes_off_static_desktop_can_still_use_the_damage_gate() {
        let base = Instant::now();
        let geometry = (1920, 1080);
        let cursor = CursorCapture::disabled();
        let gate = clean_damage_gate(base, geometry, CursorSnapshot::Disabled);
        let probes = Cell::new(0_u32);
        let decision = gate
            .decide_with_cursor_probe(
                base + Duration::from_secs(1),
                geometry,
                cursor.needs_shape(),
                || {
                    probes.set(probes.get() + 1);
                    Ok::<_, ()>(Some(CursorSnapshot::Disabled))
                },
            )
            .unwrap();
        assert_eq!(decision, DamageGateDecision::NoChange);
        assert_eq!(probes.get(), 1);
    }

    #[test]
    fn damage_gate_honors_all_visual_invalidators_including_same_size_epochs() {
        let base = Instant::now();
        let geometry = (1280, 720);
        let cursor = CursorSnapshot::Position(Some((4, 5)));
        let gate = clean_damage_gate(base, geometry, cursor);

        assert_eq!(
            gate.decide(base, (720, 1280), cursor, false),
            DamageGateDecision::Capture
        );
        assert_eq!(
            gate.decide(
                base,
                geometry,
                CursorSnapshot::Position(Some((5, 4))),
                false
            ),
            DamageGateDecision::Capture
        );
        assert_eq!(
            gate.decide(base, geometry, cursor, true),
            DamageGateDecision::Capture
        );

        // ConfigureNotify/RandR epochs explicitly invalidate Damage even if
        // the authoritative geometry query returns the same dimensions.
        let mut same_size_epoch = gate;
        same_size_epoch.invalidate();
        assert_eq!(
            same_size_epoch.decide(base, geometry, cursor, false),
            DamageGateDecision::Capture
        );
    }

    #[test]
    fn subtract_races_and_failed_readbacks_leave_damage_dirty() {
        let base = Instant::now();
        let geometry = (800, 600);
        let cursor = CursorSnapshot::Disabled;
        let mut gate = clean_damage_gate(base, geometry, cursor);

        gate.subtract_queued();
        assert!(!gate.dirty);
        gate.invalidate();
        gate.capture_succeeded(base + Duration::from_secs(1), geometry, cursor);
        assert!(
            gate.dirty,
            "a notify drained during readback must survive commit"
        );

        gate.subtract_queued();
        gate.capture_failed();
        assert!(gate.dirty);
        assert_eq!(
            gate.last_capture,
            Some(base + Duration::from_secs(1)),
            "a failed readback must not advance the force-refresh baseline"
        );
    }

    #[test]
    fn successful_frame_commits_the_cursor_it_composited_not_the_gate_probe() {
        let base = Instant::now();
        let geometry = (1024, 768);
        let probe_a = CursorSnapshot::Position(Some((10, 10)));
        let captured_b = CursorSnapshot::Position(Some((20, 20)));
        let mut gate = DamageGateState::new();
        assert_eq!(
            gate.decide(base, geometry, probe_a, false),
            DamageGateDecision::Capture
        );

        gate.subtract_queued();
        gate.capture_succeeded(base, geometry, captured_b);
        assert_eq!(gate.last_cursor, Some(captured_b));
        // A -> B during capture -> A afterwards must capture again; recording
        // the probe A here would incorrectly suppress the frame containing B.
        assert_eq!(
            gate.decide(base + Duration::from_millis(1), geometry, probe_a, false),
            DamageGateDecision::Capture
        );
    }

    #[test]
    fn damage_notifications_require_both_active_id_and_drawable() {
        let base = Instant::now();
        let active = DamageObject {
            id: 41,
            drawable: 42,
        };
        let mut tracker = DamageTracker {
            mode: DamageMode::Active(active),
            gate: clean_damage_gate(base, (640, 480), CursorSnapshot::Disabled),
        };
        let notify = |damage_id, drawable| damage::NotifyEvent {
            damage: damage_id,
            drawable,
            ..Default::default()
        };

        tracker.observe_notification(&notify(40, active.drawable));
        tracker.observe_notification(&notify(active.id, 43));
        assert!(
            !tracker.gate.dirty,
            "stale and unrelated events are ignored"
        );
        tracker.observe_notification(&notify(active.id, active.drawable));
        tracker.observe_notification(&notify(active.id, active.drawable));
        assert!(
            tracker.gate.dirty,
            "duplicate matching events are idempotent"
        );
    }

    #[test]
    fn damage_lifecycle_rebinds_and_cleanup_consumes_one_active_resource() {
        let first = DamageObject { id: 7, drawable: 8 };
        assert_eq!(
            damage_sync_action(DamageMode::Ready, Some(first.drawable)),
            DamageSyncAction::Attach(first.drawable)
        );
        assert_eq!(
            damage_sync_action(DamageMode::Active(first), Some(first.drawable)),
            DamageSyncAction::None
        );
        assert_eq!(
            damage_sync_action(DamageMode::Active(first), Some(9)),
            DamageSyncAction::Attach(9)
        );
        assert_eq!(
            damage_sync_action(DamageMode::Active(first), None),
            DamageSyncAction::Detach
        );

        let mut tracker = DamageTracker {
            mode: DamageMode::Active(first),
            gate: DamageGateState::new(),
        };
        assert_eq!(tracker.take_active_for_cleanup(), Some(first));
        assert_eq!(tracker.take_active_for_cleanup(), None);
        assert_eq!(tracker.mode, DamageMode::Disabled);
    }

    #[test]
    fn damage_setup_and_cursor_failure_policies_are_fail_safe() {
        assert!(!damage_requested(CaptureSource::Root));
        assert!(damage_requested(CaptureSource::Auto));
        assert!(damage_requested(CaptureSource::Overlay));
        assert!(!supports_damage_version(0, 9));
        assert!(supports_damage_version(1, 0));

        assert_eq!(
            CursorFailure::extension_connection(ConnectionError::UnsupportedExtension).kind,
            CursorFailureKind::Recoverable
        );
        assert_eq!(
            CursorFailure::extension_reply(ReplyError::ConnectionError(
                ConnectionError::UnsupportedExtension
            ))
            .kind,
            CursorFailureKind::Recoverable
        );
        assert_eq!(
            CursorFailure::core_connection(ConnectionError::UnsupportedExtension).kind,
            CursorFailureKind::Fatal
        );
        assert_eq!(
            CursorFailure::core_connection(ConnectionError::IoError(io::Error::other("closed")))
                .kind,
            CursorFailureKind::Fatal
        );
    }

    #[test]
    fn subtract_queue_and_async_rejection_have_distinct_failure_policies() {
        assert_eq!(
            damage_subtract_queue_action(&ConnectionError::UnsupportedExtension),
            DamageSubtractQueueAction::Fatal
        );
        assert_eq!(
            damage_subtract_queue_action(&ConnectionError::IoError(io::Error::other("closed"))),
            DamageSubtractQueueAction::Fatal
        );
        assert_eq!(
            async_x11_error_action(Some(damage::X11_EXTENSION_NAME)),
            AsyncX11ErrorAction::DisableDamage
        );
        assert_eq!(
            async_x11_error_action(Some(render::X11_EXTENSION_NAME)),
            AsyncX11ErrorAction::Fatal
        );
        assert_eq!(async_x11_error_action(None), AsyncX11ErrorAction::Fatal);
    }

    #[test]
    fn premultiplied_cursor_pixels_blend_and_clip() {
        let mut image = RgbImage::from_pixel(2, 1, image::Rgb([255, 255, 255]));
        let cursor = xfixes::GetCursorImageReply {
            width: 2,
            height: 1,
            // Half-transparent premultiplied red, then fully opaque blue.
            cursor_image: vec![0x8080_0000, 0xff00_00ff],
            ..Default::default()
        };
        CursorShape::from_reply(&cursor)
            .unwrap()
            .composite(&mut image, 0, 0, 2, 1);
        assert_eq!(image.get_pixel(0, 0).0, [255, 127, 127]);
        assert_eq!(image.get_pixel(1, 0).0, [0, 0, 255]);

        let clipped = xfixes::GetCursorImageReply {
            width: 1,
            height: 1,
            xhot: 2,
            cursor_image: vec![0xffff_ffff],
            ..Default::default()
        };
        CursorShape::from_reply(&clipped)
            .unwrap()
            .composite(&mut image, 0, 0, 2, 1);
        assert_eq!(image.get_pixel(0, 0).0, [255, 127, 127]);
    }

    #[test]
    fn scaled_cursor_uses_the_encoded_frame_coordinate_space() {
        let mut image = RgbImage::from_pixel(2, 1, image::Rgb([255, 255, 255]));
        let cursor = xfixes::GetCursorImageReply {
            width: 2,
            height: 2,
            cursor_image: vec![0xffff_0000; 4],
            ..Default::default()
        };
        CursorShape::from_reply(&cursor)
            .unwrap()
            .composite(&mut image, 2, 0, 4, 2);
        assert_eq!(image.get_pixel(0, 0).0, [255, 255, 255]);
        assert_eq!(image.get_pixel(1, 0).0, [255, 0, 0]);
    }

    #[test]
    fn cursor_serial_drives_shape_refresh_and_stale_shapes_are_suppressed() {
        let reply = xfixes::GetCursorImageReply {
            width: 1,
            height: 1,
            cursor_serial: 7,
            cursor_image: vec![0xffff_0000],
            ..Default::default()
        };
        let mut cursor = CursorCapture::new(true);
        assert!(cursor.needs_shape());
        cursor.update_shape(&reply).unwrap();
        assert!(!cursor.dirty);
        assert!(!cursor.needs_shape());

        // Redisplaying the same serial never refetches or rescales it.
        cursor.observe_serial(7);
        assert!(!cursor.dirty);
        assert!(!cursor.needs_shape());

        // If a notification and the authoritative reply disagree, do not
        // paint either shape in that frame; fetch once more next frame.
        cursor.observe_serial(8);
        let newer = xfixes::GetCursorImageReply {
            cursor_serial: 9,
            ..reply.clone()
        };
        cursor.update_shape(&newer).unwrap();
        assert!(cursor.dirty);
        let mut image = RgbImage::from_pixel(1, 1, image::Rgb([255, 255, 255]));
        cursor.composite_at(&mut image, 0, 0, 1, 1);
        assert_eq!(image.get_pixel(0, 0).0, [255, 255, 255]);
        assert!(cursor.needs_shape());

        cursor.update_shape(&newer).unwrap();
        assert!(!cursor.dirty);
        cursor.composite_at(&mut image, 0, 0, 1, 1);
        assert_eq!(image.get_pixel(0, 0).0, [255, 0, 0]);
    }

    #[test]
    fn scaled_cursor_allocation_is_reused_until_geometry_changes() {
        let reply = xfixes::GetCursorImageReply {
            width: 2,
            height: 2,
            cursor_serial: 1,
            cursor_image: vec![0xffff_ffff; 4],
            ..Default::default()
        };
        let mut shape = CursorShape::from_reply(&reply).unwrap();
        let mut half = RgbImage::new(2, 1);
        shape.composite(&mut half, 0, 0, 4, 2);
        let first = shape.scaled.as_ref().unwrap();
        let first_key = first.key;
        let first_ptr = first.image.as_raw().as_ptr();

        shape.composite(&mut half, 1, 0, 4, 2);
        let reused = shape.scaled.as_ref().unwrap();
        assert_eq!(reused.key, first_key);
        assert_eq!(reused.image.as_raw().as_ptr(), first_ptr);

        let mut different = RgbImage::new(3, 2);
        shape.composite(&mut different, 0, 0, 4, 2);
        assert_ne!(shape.scaled.as_ref().unwrap().key, first_key);
    }

    #[test]
    fn pointer_position_rejects_other_screens_and_roots() {
        let pointer = QueryPointerReply {
            same_screen: true,
            root: 10,
            root_x: -4,
            root_y: 12,
            ..Default::default()
        };
        assert_eq!(pointer_position(10, &pointer), Some((-4, 12)));
        assert_eq!(pointer_position(11, &pointer), None);
        assert_eq!(
            pointer_position(
                10,
                &QueryPointerReply {
                    same_screen: false,
                    ..pointer
                }
            ),
            None
        );
    }

    #[test]
    fn event_cache_states_invalidate_and_fall_back_independently() {
        let mut geometry = RootGeometryCache::new(1920, 1080, true);
        assert!(!geometry.dirty);
        geometry.invalidate();
        assert!(geometry.dirty);
        assert!(geometry.fall_back_to_polling());
        assert!(!geometry.event_driven);
        assert!(!geometry.fall_back_to_polling());

        let mut compositor = CompositorTracker::new(20, 30, true);
        compositor.invalidate();
        assert!(compositor.dirty);
        assert!(compositor.saw_transition);
        assert!(compositor.fall_back_to_polling());
        assert!(!compositor.event_driven);

        let mut cursor = CursorCapture::new(true);
        assert!(cursor.fall_back_to_polling());
        assert_eq!(cursor.mode, CursorMode::Polling);
        assert!(cursor.needs_shape());
        assert!(!cursor.fall_back_to_polling());

        let disabled = CursorCapture::disabled();
        assert!(!disabled.needs_shape());
    }

    #[test]
    fn root_owner_epochs_republish_without_touching_the_overlay() {
        let mut publishes = 0;
        let mut overlay_actions = 0;
        // A -> NONE -> B may be observed as two notifications or coalesced
        // before the one authoritative owner query. Either transition must
        // re-notify the compositor without acquiring/releasing its overlay.
        for owner in [x11rb::NONE, 42] {
            let decision =
                overlay_sync_decision(CaptureSource::Root, false, owner, false, true, false);
            publishes += usize::from(decision.publish_inhibitor);
            overlay_actions += usize::from(decision.action != OverlaySyncAction::None);
        }
        assert_eq!(publishes, 2);
        assert_eq!(overlay_actions, 0);

        // Auto with Composite unavailable has the same inhibitor obligation.
        let decision = overlay_sync_decision(CaptureSource::Auto, false, 42, false, true, false);
        assert!(decision.publish_inhibitor);
        assert_eq!(decision.action, OverlaySyncAction::None);
    }

    #[test]
    fn overlay_decision_releases_only_at_final_none_and_reacquires_on_epoch() {
        assert_eq!(
            overlay_sync_decision(CaptureSource::Auto, true, x11rb::NONE, true, true, false).action,
            OverlaySyncAction::Release
        );
        assert_eq!(
            overlay_sync_decision(CaptureSource::Auto, true, 42, false, true, false).action,
            OverlaySyncAction::Acquire
        );
        assert_eq!(
            overlay_sync_decision(CaptureSource::Auto, true, 42, true, true, false).action,
            OverlaySyncAction::None
        );
    }

    #[test]
    fn xfixes_capture_requires_version_one() {
        assert!(!supports_xfixes_capture(0, 99));
        assert!(supports_xfixes_capture(1, 0));
        assert!(supports_xfixes_capture(5, 0));
    }

    #[test]
    fn shm_fd_requires_protocol_version_one_two() {
        assert!(!supports_shm_fd_version(0, 99));
        assert!(!supports_shm_fd_version(1, 1));
        assert!(supports_shm_fd_version(1, 2));
        assert!(supports_shm_fd_version(1, 3));
        assert!(supports_shm_fd_version(2, 0));
    }

    #[test]
    fn shm_buffer_size_uses_native_bits_and_scanline_padding() {
        let depth_24_in_32 = Format {
            depth: 24,
            bits_per_pixel: 32,
            scanline_pad: 32,
        };
        assert_eq!(
            shm_buffer_size(1280, 536, depth_24_in_32).unwrap(),
            2_744_320
        );
        assert_eq!(shm_buffer_size(1, 1, depth_24_in_32).unwrap(), 4);
        assert_eq!(shm_buffer_size(3, 1, depth_24_in_32).unwrap(), 12);

        let padded_16 = Format {
            depth: 16,
            bits_per_pixel: 16,
            scanline_pad: 32,
        };
        // Three 16-bit pixels occupy 48 bits and round up to an 8-byte row.
        assert_eq!(shm_buffer_size(3, 2, padded_16).unwrap(), 16);
        assert!(shm_buffer_size(0, 2, padded_16).is_err());
        assert!(shm_buffer_size(2, 0, padded_16).is_err());

        let invalid = Format {
            depth: 24,
            bits_per_pixel: 32,
            scanline_pad: 24,
        };
        assert!(shm_buffer_size(1, 1, invalid).is_err());
        let invalid = Format {
            depth: 24,
            bits_per_pixel: 12,
            scanline_pad: 32,
        };
        assert!(shm_buffer_size(1, 1, invalid).is_err());

        // Valid X11 dimensions can still exceed MIT-SHM's CARD32 size.
        assert!(shm_buffer_size(u16::MAX, u16::MAX, depth_24_in_32).is_err());
    }

    #[test]
    fn shm_reply_must_exactly_match_native_image_and_mapping() {
        assert_eq!(validate_shm_reply(24, 64, 128, 24, 64).unwrap(), 64);
        assert!(validate_shm_reply(24, 64, 128, 16, 64).is_err());
        assert!(validate_shm_reply(24, 64, 128, 24, 60).is_err());
        assert!(validate_shm_reply(24, 64, 128, 24, 68).is_err());
        assert!(validate_shm_reply(24, 64, 63, 24, 64).is_err());
    }

    #[test]
    fn shm_capacity_is_reused_and_only_grows() {
        assert!(shm_needs_growth(None, 64));
        assert!(!shm_needs_growth(Some(64), 64));
        assert!(!shm_needs_growth(Some(128), 64));
        assert!(shm_needs_growth(Some(63), 64));
    }

    #[test]
    fn shm_failure_uses_same_frame_core_before_disabling() {
        let calls = Cell::new(0);
        let outcome = resolve_readback(None, || {
            calls.set(calls.get() + 1);
            Ok::<_, &'static str>(5)
        });
        assert_eq!(calls.get(), 1);
        assert!(matches!(outcome, ReadbackOutcome::Image(5)));

        let calls = Cell::new(0);
        let outcome = resolve_readback(Some(Err("shm")), || {
            calls.set(calls.get() + 1);
            Ok::<_, &'static str>(7)
        });
        assert_eq!(calls.get(), 1);
        match outcome {
            ReadbackOutcome::CoreFallback { image, shm_error } => {
                assert_eq!(image, 7);
                assert_eq!(shm_error, "shm");
            }
            _ => panic!("SHM failure with a valid drawable must use core fallback"),
        }

        let calls = Cell::new(0);
        let outcome = resolve_readback(Some(Ok::<_, &'static str>(9)), || {
            calls.set(calls.get() + 1);
            Ok(10)
        });
        assert_eq!(calls.get(), 0);
        assert!(matches!(outcome, ReadbackOutcome::Image(9)));

        let calls = Cell::new(0);
        let outcome = resolve_readback(Some(Err("shm")), || {
            calls.set(calls.get() + 1);
            Err::<u8, _>("core")
        });
        assert_eq!(calls.get(), 1);
        assert!(matches!(outcome, ReadbackOutcome::Error("core")));
    }
}
