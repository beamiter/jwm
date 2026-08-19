//! Read the final X11 compositor surface through the Composite overlay.
//!
//! This is intentionally kept in the out-of-process LAN MVP.  A slow encoder
//! or peer can therefore never stall JWM's display event loop.  Both the
//! x11rb and xcb JWM backends render into the same X Composite overlay, so one
//! small X11 client covers both transports.

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
use x11rb::image::{BitsPerPixel, Image as XImage, ImageOrder, PixelLayout, ScanlinePad};
use x11rb::protocol::render::{CreatePictureAux, PictOp, Pictformat, Picture, Repeat, Transform};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ChangeWindowAttributesAux, ConnectionExt as _, CreateWindowAux, EventMask,
    Format, ImageFormat, Pixmap, PropMode, QueryPointerReply, Screen, VisualClass, Visualid,
    Visualtype, Window, WindowClass,
};
use x11rb::protocol::{Event, composite, randr, render, shm, xfixes};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::x11_utils::TryParseFd;

const COMPOSITE_CLIENT_VERSION: (u32, u32) = (0, 4);
const COMPOSITE_OVERLAY_VERSION: (u32, u32) = (0, 3);
const XFIXES_CLIENT_VERSION: (u32, u32) = (5, 0);
const RANDR_CLIENT_VERSION: (u32, u32) = (1, 6);
const RENDER_CLIENT_VERSION: (u32, u32) = (0, 11);
const RENDER_TRANSFORM_VERSION: (u32, u32) = (0, 10);
const SHM_FD_VERSION: (u16, u16) = (1, 2);
const REMOTE_CAPTURE_OWNER: &[u8] = b"_JWM_REMOTE_CAPTURE_OWNER";
const OVERLAY_RETRY_DELAY: Duration = Duration::from_secs(1);

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
        let render_scaler = if max_width == 0 || requested_source == CaptureSource::Root {
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

        let mut capture = Self {
            conn,
            screen_num,
            root,
            drawable,
            overlay_acquired,
            next_overlay_retry,
            compositor,
            composite_ready,
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
        Ok(capture)
    }

    /// Capture and optionally downscale a frame.
    ///
    /// `GetImage` is synchronous, but this process is deliberately separate
    /// from JWM.  The compositor event loop is never made to wait for JPEG or
    /// network I/O. The synchronous server readback can still contend with
    /// the compositor on very large roots, which is why the MVP exposes a
    /// conservative frame-rate default.
    pub fn frame(&mut self) -> RemoteResult<CapturedFrame> {
        self.drain_dynamic_events()?;
        self.sync_overlay_source()?;
        let (mut source_width, mut source_height) =
            self.root_geometry.dimensions(&self.conn, self.root)?;
        validate_root_geometry(source_width, source_height)?;

        let mut drawable = self.drawable;
        let mut allow_render = true;
        let mut dynamic_retry_available = true;
        loop {
            match self.capture_drawable(drawable, source_width, source_height, allow_render) {
                Ok(frame) => return Ok(frame),
                Err(failure) if failure.kind == CaptureFailureKind::Fatal => {
                    return Err(failure.error);
                }
                Err(failure) => {
                    if dynamic_retry_available {
                        if let Some(reconciled) = self.reconcile_after_capture_failure(
                            drawable,
                            source_width,
                            source_height,
                        )? {
                            dynamic_retry_available = false;
                            drawable = reconciled.drawable;
                            source_width = reconciled.width;
                            source_height = reconciled.height;
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
                                "remote: compositor overlay readback failed ({}); switching to root capture",
                                failure.error
                            );
                            self.release_overlay();
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
        allow_render: bool,
    ) -> Result<CapturedFrame, CaptureFailure> {
        let (output_width, output_height) =
            scaled_dimensions(source_width, source_height, self.max_width);
        // A Window used directly as an XRender source does not reliably
        // include its child windows. The Composite overlay has the final
        // composited pixels, while root fallback deliberately keeps the
        // proven GetImage + CPU resize path.
        if drawable != self.root
            && (output_width != source_width || output_height != source_height)
            && self.render_scaler.is_some()
            && allow_render
        {
            let pending_cursor = self
                .cursor
                .prepare(&self.conn, self.root)
                .map_err(CaptureFailure::fatal)?;
            let render_result = self
                .render_scaler
                .as_mut()
                .expect("Render scaler presence checked above")
                .capture(
                    &self.conn,
                    &mut self.shm_readback,
                    drawable,
                    source_width,
                    source_height,
                    output_width,
                    output_height,
                );
            match render_result {
                Ok(mut image) => {
                    let position = resolve_pending_cursor(
                        &self.conn,
                        self.root,
                        &mut self.cursor,
                        pending_cursor,
                    );
                    self.finish_cursor(position, &mut image, source_width, source_height)
                        .map_err(CaptureFailure::fatal)?;
                    return Ok(CapturedFrame {
                        image,
                        source_width,
                        source_height,
                    });
                }
                Err(error) => {
                    pending_cursor.discard();
                    return Err(CaptureFailure::render(error));
                }
            }
        }

        let root_depth = self.screen().map_err(CaptureFailure::fatal)?.root_depth;
        let pending_cursor = self
            .cursor
            .prepare(&self.conn, self.root)
            .map_err(CaptureFailure::fatal)?;
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
            resolve_pending_cursor(&self.conn, self.root, &mut self.cursor, pending_cursor);
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

        Ok(CapturedFrame {
            image,
            source_width,
            source_height,
        })
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

    fn sync_overlay_source(&mut self) -> RemoteResult<()> {
        self.sync_overlay_source_with_force(false).map(|_| ())
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
                self.release_overlay();
                if self.requested_source == CaptureSource::Overlay {
                    return Err(invalid_data("X11 compositor stopped during remote capture").into());
                }
            }
            OverlaySyncAction::Acquire => match acquire_overlay(&self.conn, self.root) {
                Ok(overlay) => {
                    self.drawable = overlay;
                    self.overlay_acquired = true;
                    self.next_overlay_retry = None;
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
    ) -> RemoteResult<Option<ReconciledCapture>> {
        self.drain_dynamic_events()?;
        let source_transition = self.sync_overlay_source_with_force(true)?;
        let (width, height) = self
            .root_geometry
            .refresh_authoritative(&self.conn, self.root)?;
        let changed = source_transition
            || self.drawable != attempted_drawable
            || width != attempted_width
            || height != attempted_height;
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
                }
                Event::RandrScreenChangeNotify(event) if event.root == self.root => {
                    // RandR reports the unrotated screen size in this event.
                    // Treat it only as an invalidator and query root geometry.
                    self.root_geometry.invalidate();
                }
                Event::XfixesCursorNotify(event) if event.window == self.root => {
                    self.cursor.observe_serial(event.cursor_serial);
                }
                Event::XfixesSelectionNotify(event) if event.window == self.root => {
                    if event.selection == self.compositor.selection() {
                        self.compositor.invalidate();
                    }
                }
                Event::Error(error) => {
                    return Err(io::Error::other(format!(
                        "asynchronous X11 error while capturing: {error:?}"
                    ))
                    .into());
                }
                Event::Unknown(_) => {
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

    fn release_overlay(&mut self) {
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

fn resolve_pending_cursor(
    conn: &RustConnection,
    root: Window,
    cursor: &mut CursorCapture,
    pending: PendingCursor<'_>,
) -> Option<(i32, i32)> {
    match cursor.resolve(conn, root, pending) {
        Ok(position) => position,
        Err(error) => {
            eprintln!("remote: cursor capture stopped: {error}");
            cursor.disable();
            None
        }
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
}

impl RootGeometryCache {
    fn new(width: u16, height: u16, event_driven: bool) -> Self {
        Self {
            width,
            height,
            event_driven,
            dirty: false,
        }
    }

    fn invalidate(&mut self) {
        self.dirty = true;
    }

    fn fall_back_to_polling(&mut self) -> bool {
        if !self.event_driven {
            return false;
        }
        self.event_driven = false;
        self.dirty = true;
        true
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
        self.width = geometry.width;
        self.height = geometry.height;
        self.dirty = false;
        Ok((self.width, self.height))
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
        self.mode == CursorMode::Polling || self.dirty || self.shape.is_none()
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
    ) -> RemoteResult<PendingCursor<'a>> {
        match self.mode {
            CursorMode::Disabled => Ok(PendingCursor::Disabled),
            // Without cursor notifications, fetch both position and pixels
            // after readback to preserve the reliable legacy snapshot path.
            CursorMode::Polling => Ok(PendingCursor::Polling),
            CursorMode::EventDriven => {
                let pointer = conn.query_pointer(root)?;
                let shape = if self.needs_shape() {
                    match xfixes::get_cursor_image(conn) {
                        Ok(shape) => Some(shape),
                        Err(error) => {
                            pointer.discard_reply_and_errors();
                            return Err(error.into());
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
    ) -> RemoteResult<Option<(i32, i32)>> {
        let pointer = match pending {
            PendingCursor::Disabled => return Ok(None),
            PendingCursor::Polling => {
                let pointer = conn.query_pointer(root)?;
                let shape = match xfixes::get_cursor_image(conn) {
                    Ok(shape) => shape,
                    Err(error) => {
                        pointer.discard_reply_and_errors();
                        return Err(error.into());
                    }
                };
                let pointer = match pointer.reply() {
                    Ok(pointer) => pointer,
                    Err(error) => {
                        shape.discard_reply_and_errors();
                        return Err(error.into());
                    }
                };
                self.update_shape(&shape.reply()?)?;
                pointer
            }
            PendingCursor::EventDriven { pointer, shape } => {
                let pointer = match pointer.reply() {
                    Ok(pointer) => pointer,
                    Err(error) => {
                        if let Some(shape) = shape {
                            shape.discard_reply_and_errors();
                        }
                        return Err(error.into());
                    }
                };
                if let Some(shape) = shape {
                    self.update_shape(&shape.reply()?)?;
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

struct RenderSource {
    drawable: Window,
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

        Ok(Self {
            visual_formats,
            root: screen.root,
            root_depth: screen.root_depth,
            root_format,
            root_layout: PixelLayout::from_visual_type(root_visual)?,
            root_standard_bgrx_visual: is_standard_bgrx_visual(root_visual),
            source: None,
            target: None,
            reported_dimensions: None,
        })
    }

    fn capture(
        &mut self,
        conn: &RustConnection,
        readback: &mut ShmReadback,
        drawable: Window,
        source_width: u16,
        source_height: u16,
        output_width: u16,
        output_height: u16,
    ) -> RemoteResult<RgbImage> {
        self.ensure_target(conn, output_width, output_height)?;
        self.ensure_source(conn, drawable)?;
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
        let target = self
            .target
            .as_ref()
            .ok_or_else(|| invalid_data("XRender target picture was not created"))?;
        let composite = render::composite(
            conn,
            PictOp::SRC,
            source.picture,
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
        )?;
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
        )?;
        // Waiting for ShmGetImage or core GetImage also advances the connection beyond the
        // preceding Composite request. Checking its cookie now reports a
        // precise Render error without adding another round trip normally.
        composite.check()?;
        if self.reported_dimensions != Some(dimensions) {
            eprintln!(
                "jwm-remote: XRender downscale {}x{} -> {}x{}",
                source_width, source_height, output_width, output_height
            );
            self.reported_dimensions = Some(dimensions);
        }
        Ok(image)
    }

    fn ensure_source(&mut self, conn: &RustConnection, drawable: Window) -> RemoteResult<()> {
        if self
            .source
            .as_ref()
            .is_some_and(|source| source.drawable == drawable)
        {
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
        if let Err(error) = render::set_picture_filter(conn, picture, b"bilinear", &[])?.check() {
            let _ = render::free_picture(conn, picture);
            return Err(error.into());
        }

        let old = self.source.replace(RenderSource {
            drawable,
            picture,
            transform: None,
        });
        if let Some(old) = old {
            let _ = render::free_picture(conn, old.picture);
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
        conn.create_pixmap(self.root_depth, pixmap, self.root, width, height)?
            .check()?;
        let picture = conn.generate_id()?;
        let create = render::create_picture(
            conn,
            picture,
            pixmap,
            self.root_format,
            &CreatePictureAux::new(),
        )?
        .check();
        if let Err(error) = create {
            let _ = conn.free_pixmap(pixmap);
            return Err(error.into());
        }

        let old = self.target.replace(RenderTarget {
            pixmap,
            picture,
            width,
            height,
        });
        if let Some(old) = old {
            let _ = render::free_picture(conn, old.picture);
            let _ = conn.free_pixmap(old.pixmap);
        }
        Ok(())
    }

    fn release_source(&mut self, conn: &RustConnection) {
        if let Some(source) = self.source.take() {
            let _ = render::free_picture(conn, source.picture);
        }
    }

    fn release(&mut self, conn: &RustConnection) {
        self.release_source(conn);
        if let Some(target) = self.target.take() {
            let _ = render::free_picture(conn, target.picture);
            let _ = conn.free_pixmap(target.pixmap);
        }
    }
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
        self.release_overlay();
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
        assert!(!cursor.fall_back_to_polling());
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
