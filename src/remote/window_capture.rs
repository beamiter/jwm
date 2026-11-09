//! Fail-closed, view-only capture of one authorized X11 window instance.
//!
//! Unlike desktop capture, this backend never reads the root, an overlay, or
//! screen coordinates. The only pixel source is a Composite named pixmap of
//! the originally authorized live window. Keep this object alive from
//! authorization through the (single) viewer session: reconstructing it from
//! the numeric XID on reconnect would authorize an unrelated reused XID.
//!
//! This version revokes on unmap or reparent. Local resize replaces the named
//! pixmap only after ordered lifecycle and geometry validation. It
//! excludes global cursor/clipboard/input and rejects alpha visuals. Window
//! remote-requested resize and application-owned popups remain separate work.

use super::RemoteResult;
use super::messages::FrameLimits;
use super::window_resize_native::NativeResizeGrant;
use super::x11_capture::{
    CaptureImageRecycler, CaptureOutcome, CapturedFrame, decode_ximage_reusing, frame_dimensions,
    open_display, wait_readable,
};
use image::imageops::FilterType;
use std::io;
use std::os::fd::AsFd;
use std::time::Duration;
use x11rb::connection::Connection;
use x11rb::image::{Image as XImage, PixelLayout};
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    Atom, ChangeWindowAttributesAux, ConnectionExt as _, EventMask, ImageFormat, MapState, Pixmap,
    VisualClass, Window, WindowClass,
};
use x11rb::protocol::{composite, xfixes};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

/// Terminal grant state. No event, including a later Map/Create of the same
/// XID, can bring a revoked instance back to life.
#[derive(Debug)]
struct WindowLifetime {
    window: Window,
    /// Immutable ancestor chain subscribed before the grant becomes usable.
    ancestors: Vec<Window>,
    width: u16,
    height: u16,
    revoked: bool,
    observed_size: (u16, u16),
    revision: u64,
    needs_rebind: bool,
}

impl WindowLifetime {
    fn covers(&self, window: Window) -> bool {
        window == self.window || self.ancestors.contains(&window)
    }

    fn observe(&mut self, event: &Event) {
        let invalid = match event {
            Event::DestroyNotify(e) => self.covers(e.window),
            Event::UnmapNotify(e) => self.covers(e.window),
            Event::ReparentNotify(e) => self.covers(e.window),
            Event::ConfigureNotify(e) if e.window == self.window => {
                self.observe_size((e.width, e.height));
                e.border_width != 0
            }
            _ => false,
        };
        self.revoked |= invalid;
    }

    fn observe_size(&mut self, size: (u16, u16)) {
        if size != self.observed_size {
            self.observed_size = size;
            self.needs_rebind = true;
            match self.revision.checked_add(1) {
                Some(revision) => self.revision = revision,
                None => self.revoked = true,
            }
        }
    }

    fn check(&self) -> RemoteResult<()> {
        if self.revoked {
            Err(invalid("window authorization expired; select the window again locally").into())
        } else {
            Ok(())
        }
    }
}

pub(super) struct WindowCapture {
    conn: RustConnection,
    native_resize: Option<NativeResizeGrant>,
    resize_challenge: Option<(Atom, [u8; 32])>,
    pixmap: Pixmap,
    lifetime: WindowLifetime,
    compositor_selection: Atom,
    compositor_owner: Window,
    layout: PixelLayout,
    host_width_limit: u16,
    max_width: u16,
    max_height: u16,
    limits: FrameLimits,
    epoch: u64,
    recycler: CaptureImageRecycler,
}

impl WindowCapture {
    /// Authorize the currently mapped window, not its title or future XID.
    /// Requires an existing compositor and an already redirected window; this
    /// operation does not redirect windows or change compositor ownership.
    pub(super) fn connect(
        display: Option<&str>,
        window: Window,
        max_width: u16,
    ) -> RemoteResult<Self> {
        let (conn, screen_num) = open_display(display)?;
        let screen = conn
            .setup()
            .roots
            .get(screen_num)
            .ok_or_else(|| invalid("X11 selected an unavailable screen"))?;
        let root = screen.root;
        if window == 0 || conn.setup().roots.iter().any(|s| s.root == window) {
            return Err(invalid("window capture requires a non-root window").into());
        }
        let version = composite::query_version(&conn, 0, 4)?.reply()?;
        if (version.major_version, version.minor_version) < (0, 2) {
            return Err(invalid("window capture requires XComposite 0.2 or newer").into());
        }
        let selection = conn
            .intern_atom(false, format!("_NET_WM_CM_S{screen_num}").as_bytes())?
            .reply()?
            .atom;
        let fixes = xfixes::query_version(&conn, 5, 0)?.reply()?;
        if fixes.major_version < 1 {
            return Err(invalid("window capture requires XFixes selection notifications").into());
        }
        xfixes::select_selection_input(
            &conn,
            root,
            selection,
            xfixes::SelectionEventMask::SET_SELECTION_OWNER
                | xfixes::SelectionEventMask::SELECTION_WINDOW_DESTROY
                | xfixes::SelectionEventMask::SELECTION_CLIENT_CLOSE,
        )?
        .check()?;
        let owner = conn.get_selection_owner(selection)?.reply()?.owner;
        if owner == 0 {
            return Err(invalid("window capture requires an active X11 compositor").into());
        }
        // Subscribe each node before reading its parent. Any reparent/destroy
        // that could invalidate the discovered chain is therefore queued on
        // a node already watched. Establish the grant only after the complete
        // chain is subscribed and the authoritative baseline has been read.
        let ancestors = subscribe_ancestry(&conn, window, root)?;
        conn.change_window_attributes(
            owner,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::STRUCTURE_NOTIFY),
        )?
        .check()?;
        let attributes = conn.get_window_attributes(window)?.reply()?;
        let geometry = conn.get_geometry(window)?.reply()?;
        if attributes.class != WindowClass::INPUT_OUTPUT
            || attributes.map_state != MapState::VIEWABLE
        {
            return Err(invalid("window capture requires a mapped InputOutput window").into());
        }
        if geometry.border_width != 0 {
            return Err(invalid("window capture requires a borderless client window").into());
        }
        if geometry.root != root {
            return Err(invalid("window belongs to another X11 screen").into());
        }
        super::frame::validate_dimensions(geometry.width, geometry.height)?;
        let visual = screen
            .allowed_depths
            .iter()
            .flat_map(|d| &d.visuals)
            .find(|v| v.visual_id == attributes.visual)
            .ok_or_else(|| invalid("window visual is unavailable"))?;
        // No desktop background is ever sampled to resolve transparency.
        // Alpha-aware fixed-background compositing is deferred intentionally.
        if geometry.depth != 24 || visual.class != VisualClass::TRUE_COLOR {
            return Err(invalid(
                "window capture currently supports opaque 24-bit TrueColor windows only",
            )
            .into());
        }
        let layout = PixelLayout::from_visual_type(*visual)?;
        let pixmap = conn.generate_id()?;
        composite::name_window_pixmap(&conn, window, pixmap)?.check()?;
        let mut capture = Self {
            conn,
            native_resize: None,
            resize_challenge: None,
            pixmap,
            lifetime: WindowLifetime {
                window,
                ancestors,
                width: geometry.width,
                height: geometry.height,
                revoked: false,
                observed_size: (geometry.width, geometry.height),
                revision: 0,
                needs_rebind: false,
            },
            compositor_selection: selection,
            compositor_owner: owner,
            layout,
            host_width_limit: max_width,
            max_width,
            max_height: 0,
            limits: FrameLimits::default(),
            epoch: 1,
            recycler: CaptureImageRecycler::default(),
        };
        capture.verify_live()?;
        Ok(capture)
    }

    pub(super) fn enable_native_resize(&mut self) -> RemoteResult<()> {
        self.verify_live()?;
        let atom = self
            .conn
            .intern_atom(false, b"_JWM_REMOTE_RESIZE_CHALLENGE")?
            .reply()?
            .atom;
        let old = self
            .conn
            .get_property(
                false,
                self.lifetime.window,
                atom,
                x11rb::protocol::xproto::AtomEnum::ANY,
                0,
                9,
            )?
            .reply()?;
        if old.type_ != 0 {
            return Err(invalid("window already has a resize lease").into());
        }
        let nonce: [u8; 32] = rand::random();
        self.conn
            .change_property8(
                x11rb::protocol::xproto::PropMode::REPLACE,
                self.lifetime.window,
                atom,
                x11rb::protocol::xproto::AtomEnum::STRING,
                &nonce,
            )?
            .check()?;
        self.resize_challenge = Some((atom, nonce));
        let result = (|| {
            // The write may have hit a replacement XID. Never grant until the
            // original watcher has consumed all earlier lifecycle events.
            self.verify_live()?;
            let grant = NativeResizeGrant::connect(self.lifetime.window, nonce)?;
            self.verify_live()?;
            self.native_resize = Some(grant);
            Ok(())
        })();
        if result.is_err() {
            self.disable_native_resize();
        }
        result
    }

    pub(super) fn disable_native_resize(&mut self) {
        self.native_resize = None;
        let Some((atom, nonce)) = self.resize_challenge.take() else {
            return;
        };
        // Compare+delete is one X server critical section, with no IPC wait.
        let cleanup = (|| -> RemoteResult<()> {
            self.conn.grab_server()?.check()?;
            let result = (|| -> RemoteResult<()> {
                let value = self
                    .conn
                    .get_property(
                        false,
                        self.lifetime.window,
                        atom,
                        x11rb::protocol::xproto::AtomEnum::STRING,
                        0,
                        9,
                    )?
                    .reply()?;
                if value.format == 8 && value.bytes_after == 0 && value.value == nonce {
                    self.conn
                        .delete_property(self.lifetime.window, atom)?
                        .check()?;
                }
                Ok(())
            })();
            let release = self.conn.ungrab_server()?.check();
            self.conn.flush()?;
            result?;
            release?;
            Ok(())
        })();
        let _ = cleanup;
    }

    pub(super) fn request_native_resize(
        &mut self,
        expected: (u16, u16),
        target: (u16, u16),
    ) -> RemoteResult<(u16, u16)> {
        let result = (|| {
            let actual = self.verify_live()?;
            if actual != expected || self.lifetime.needs_rebind {
                return Err(invalid("native resize geometry became stale").into());
            }
            self.native_resize
                .as_mut()
                .ok_or_else(|| invalid("native resize unavailable"))?
                .apply(expected, target)
        })();
        if result.is_err() {
            self.disable_native_resize();
        }
        result
    }

    fn drain_events(&mut self) -> RemoteResult<bool> {
        self.lifetime.check()?;
        let mut observed = false;
        while let Some(event) = self.conn.poll_for_event()? {
            observed = true;
            self.lifetime.observe(&event);
            if matches!(&event, Event::DestroyNotify(e) if e.window == self.compositor_owner)
                || matches!(&event, Event::XfixesSelectionNotify(e) if e.selection == self.compositor_selection)
                || matches!(&event, Event::Error(_) | Event::Unknown(_))
            {
                self.lifetime.revoked = true;
            }
        }
        self.lifetime.check()?;
        Ok(observed)
    }

    /// Ordered server reply, then drain queued lifecycle events. Never publish
    /// a readback for which we already observed invalidation. Pixels sent
    /// before a later server event cannot be retracted from a remote viewer.
    fn verify_live(&mut self) -> RemoteResult<(u16, u16)> {
        self.lifetime.check()?;
        let owner = self
            .conn
            .get_selection_owner(self.compositor_selection)?
            .reply()?
            .owner;
        if owner != self.compositor_owner {
            self.lifetime.revoked = true;
        }
        // Viewability is a current-state check, not lifecycle history. The
        // ancestor subscriptions also catch unmap/map between these queries.
        let attributes = self
            .conn
            .get_window_attributes(self.lifetime.window)?
            .reply()?;
        let geometry = self.conn.get_geometry(self.lifetime.window)?.reply()?;
        if attributes.map_state != MapState::VIEWABLE
            || geometry.depth != 24
            || geometry.border_width != 0
        {
            self.lifetime.revoked = true;
        }
        super::frame::validate_dimensions(geometry.width, geometry.height)?;
        let size = (geometry.width, geometry.height);
        self.lifetime.observe_size(size);
        self.drain_events()?;
        Ok(size)
    }

    /// Retire the stale backing store without ever reacquiring permission.
    /// The original connection and immutable ancestor watch remain active.
    /// A resize racing either query/name/readback discards the candidate; a
    /// lifecycle revocation is terminal even if the XID was reused meanwhile.
    fn rebind(&mut self, size: (u16, u16)) -> RemoteResult<bool> {
        self.rebind_after_name(size, || {})
    }

    fn rebind_after_name(
        &mut self,
        size: (u16, u16),
        after_name: impl FnOnce(),
    ) -> RemoteResult<bool> {
        self.lifetime.check()?;
        let revision = self.lifetime.revision;
        let candidate = self.conn.generate_id()?;
        composite::name_window_pixmap(&self.conn, self.lifetime.window, candidate)?.check()?;
        after_name();
        let validated = (|| -> RemoteResult<bool> {
            let geometry = self.conn.get_geometry(candidate)?.reply()?;
            let current = self.verify_live()?;
            Ok(self.lifetime.revision == revision
                && current == size
                && self.lifetime.observed_size == size
                && (geometry.width, geometry.height) == size
                && geometry.depth == 24)
        })();
        if !matches!(validated, Ok(true)) {
            if let Ok(cookie) = self.conn.free_pixmap(candidate) {
                let _ = cookie.check();
            }
            return validated;
        }
        if let Err(error) = self.advance_epoch() {
            if let Ok(cookie) = self.conn.free_pixmap(candidate) {
                let _ = cookie.check();
            }
            return Err(error);
        }
        let old = std::mem::replace(&mut self.pixmap, candidate);
        self.lifetime.width = size.0;
        self.lifetime.height = size.1;
        self.lifetime.needs_rebind = false;
        self.conn.free_pixmap(old)?.check()?;
        Ok(true)
    }

    /// Deterministic integration-test seam at the server's name/query boundary.
    /// Production passes a no-op and cannot supply callbacks through the API.
    #[cfg(test)]
    pub(super) fn rebind_with_test_hook(
        &mut self,
        after_name: impl FnOnce(),
    ) -> RemoteResult<bool> {
        let result = (|| {
            let size = self.verify_live()?;
            self.rebind_after_name(size, after_name)
        })();
        if result.is_err() {
            self.lifetime.revoked = true;
        }
        result
    }

    fn advance_epoch(&mut self) -> RemoteResult<()> {
        match self.epoch.checked_add(1) {
            Some(epoch) => {
                self.epoch = epoch;
                Ok(())
            }
            None => {
                self.lifetime.revoked = true;
                Err(invalid("window geometry epoch exhausted").into())
            }
        }
    }

    pub(super) fn frame(&mut self) -> RemoteResult<CaptureOutcome> {
        let result = self.read_frame();
        if result.is_err() {
            // Readback, decoding and connection failures are terminal too;
            // there is no desktop fallback or authorization reacquisition.
            self.lifetime.revoked = true;
        }
        result
    }

    fn read_frame(&mut self) -> RemoteResult<CaptureOutcome> {
        let size = self.verify_live()?;
        if self.lifetime.needs_rebind && !self.rebind(size)? {
            return Ok(CaptureOutcome::NoChange);
        }
        let (width, height) = (self.lifetime.width, self.lifetime.height);
        let (out_width, out_height) =
            frame_dimensions(width, height, self.max_width, self.max_height, self.limits);
        self.limits.validate_dimensions(out_width, out_height)?;
        let reply = self
            .conn
            .get_image(
                ImageFormat::Z_PIXMAP,
                self.pixmap,
                0,
                0,
                width,
                height,
                u32::MAX,
            )?
            .reply()?;
        if reply.depth != 24 {
            return Err(invalid("authorized window pixmap depth changed").into());
        }
        // GetImage on pixmaps supplies visual NONE. Decode using the target
        // window's validated visual, never the root visual or reply visual.
        let image = XImage::get_from_reply(self.conn.setup(), width, height, reply)?;
        let rgb = decode_ximage_reusing(
            &image,
            width,
            height,
            self.layout,
            false,
            &mut self.recycler,
        )?;
        let rgb = if (out_width, out_height) == (width, height) {
            rgb
        } else {
            let scaled = image::imageops::resize(
                &rgb,
                out_width.into(),
                out_height.into(),
                FilterType::Triangle,
            );
            self.recycler.recycle_image(rgb);
            scaled
        };
        self.verify_live()?;
        if self.lifetime.needs_rebind {
            self.recycler.recycle_image(rgb);
            return Ok(CaptureOutcome::NoChange);
        }
        Ok(CaptureOutcome::Frame(CapturedFrame {
            geometry_epoch: self.epoch,
            image: rgb,
            source_width: width,
            source_height: height,
        }))
    }

    pub(super) fn set_frame_limits(&mut self, limits: FrameLimits) -> bool {
        let limits = self.limits.intersect(limits);
        if limits.validate().is_err() || limits == self.limits {
            return false;
        }
        self.limits = limits;
        self.advance_epoch().is_ok()
    }

    pub(super) fn set_encoded_bounds(&mut self, width: u16, height: u16) -> bool {
        // Viewport shrink is not a permanent authorization reduction. A
        // later enlargement may recover detail up to the original host cap.
        let width = narrow_bound(self.host_width_limit, width);
        if (width, height) == (self.max_width, self.max_height) {
            return false;
        }
        self.max_width = width;
        self.max_height = height;
        self.advance_epoch().is_ok()
    }

    #[must_use]
    pub(super) fn geometry_epoch(&self) -> u64 {
        self.epoch
    }

    pub(crate) fn image_recycler(&self) -> CaptureImageRecycler {
        self.recycler.clone()
    }

    pub(super) fn wait_for_activity(&mut self, timeout: Duration) -> RemoteResult<bool> {
        let result = (|| {
            if self.drain_events()? {
                return Ok(true);
            }
            if !wait_readable(self.conn.stream().as_fd(), timeout)? {
                return Ok(false);
            }
            self.drain_events()
        })();
        if result.is_err() {
            self.lifetime.revoked = true;
        }
        result
    }
}

impl Drop for WindowCapture {
    fn drop(&mut self) {
        self.disable_native_resize();
        if let Ok(cookie) = self.conn.free_pixmap(self.pixmap) {
            let _ = cookie.check();
        }
        let _ = self.conn.flush();
    }
}

/// Bounded discovery prevents a malformed server tree from allocating or
/// looping indefinitely. The root is watched too, but its size changes do not
/// revoke the window grant. No ancestor pixels are ever captured.
fn subscribe_ancestry(
    conn: &RustConnection,
    window: Window,
    root: Window,
) -> RemoteResult<Vec<Window>> {
    const MAX_CHAIN: usize = 128;
    let mut chain = Vec::new();
    let mut current = window;
    loop {
        if current == 0 || chain.contains(&current) || chain.len() >= MAX_CHAIN {
            return Err(invalid("window ancestry is invalid or exceeds the safety limit").into());
        }
        conn.change_window_attributes(
            current,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::STRUCTURE_NOTIFY),
        )?
        .check()?;
        chain.push(current);
        if current == root {
            break;
        }
        let tree = conn.query_tree(current)?.reply()?;
        if tree.root != root {
            return Err(invalid("window ancestry belongs to another screen").into());
        }
        current = tree.parent;
    }
    chain.remove(0);
    Ok(chain)
}

fn narrow_bound(current: u16, requested: u16) -> u16 {
    match (current, requested) {
        (0, n) => n,
        (n, 0) => n,
        (a, b) => a.min(b),
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use x11rb::protocol::xproto::{
        ConfigureNotifyEvent, DestroyNotifyEvent, MapNotifyEvent, ReparentNotifyEvent,
        UnmapNotifyEvent,
    };

    fn grant() -> WindowLifetime {
        WindowLifetime {
            window: 42,
            ancestors: vec![7, 8],
            width: 800,
            height: 600,
            revoked: false,
            observed_size: (800, 600),
            revision: 0,
            needs_rebind: false,
        }
    }

    #[test]
    fn destroy_is_terminal_even_if_same_xid_is_mapped_again() {
        let mut state = grant();
        state.observe(&Event::DestroyNotify(DestroyNotifyEvent {
            window: 42,
            ..Default::default()
        }));
        assert!(state.check().is_err());
        state.observe(&Event::MapNotify(MapNotifyEvent {
            window: 42,
            ..Default::default()
        }));
        assert!(state.check().is_err());
    }

    #[test]
    fn unmap_and_reparent_revoke_the_grant() {
        for event in [
            Event::UnmapNotify(UnmapNotifyEvent {
                window: 42,
                ..Default::default()
            }),
            Event::ReparentNotify(ReparentNotifyEvent {
                window: 42,
                ..Default::default()
            }),
        ] {
            let mut state = grant();
            state.observe(&event);
            assert!(state.check().is_err());
        }
    }

    #[test]
    fn moving_is_safe_but_resize_requires_fresh_pixmap() {
        let mut state = grant();
        state.observe(&Event::ConfigureNotify(ConfigureNotifyEvent {
            window: 42,
            x: 500,
            y: 200,
            width: 800,
            height: 600,
            ..Default::default()
        }));
        assert!(state.check().is_ok());
        state.observe(&Event::ConfigureNotify(ConfigureNotifyEvent {
            window: 42,
            width: 801,
            height: 600,
            ..Default::default()
        }));
        assert!(state.check().is_ok());
        assert!(state.needs_rebind);
        assert_eq!(state.revision, 1);
    }

    #[test]
    fn ancestor_unmap_then_map_permanently_revokes() {
        // Neither event needs to mention the client window itself. A final
        // VIEWABLE query cannot undo the observed loss of authorization.
        for ancestor in [7, 8] {
            let mut state = grant();
            state.observe(&Event::UnmapNotify(UnmapNotifyEvent {
                window: ancestor,
                ..Default::default()
            }));
            state.observe(&Event::MapNotify(MapNotifyEvent {
                window: ancestor,
                ..Default::default()
            }));
            assert!(state.check().is_err());
        }
    }

    #[test]
    fn ancestor_destroy_or_reparent_revokes_without_client_events() {
        for event in [
            Event::DestroyNotify(DestroyNotifyEvent {
                window: 7,
                ..Default::default()
            }),
            Event::ReparentNotify(ReparentNotifyEvent {
                window: 8,
                ..Default::default()
            }),
        ] {
            let mut state = grant();
            state.observe(&event);
            assert!(state.check().is_err());
        }
    }

    #[test]
    fn ancestor_geometry_change_does_not_change_client_pixmap() {
        let mut state = grant();
        state.observe(&Event::ConfigureNotify(ConfigureNotifyEvent {
            window: 7,
            width: 1200,
            height: 900,
            ..Default::default()
        }));
        assert!(state.check().is_ok());
    }

    #[test]
    fn other_window_events_do_not_revoke() {
        let mut state = grant();
        state.observe(&Event::DestroyNotify(DestroyNotifyEvent {
            window: 43,
            ..Default::default()
        }));
        assert!(state.check().is_ok());
    }

    #[test]
    fn viewport_can_expand_again_but_never_exceed_original_host_bounds() {
        assert_eq!(narrow_bound(1920, 3840), 1920);
        assert_eq!(narrow_bound(1920, 0), 1920);
        assert_eq!(narrow_bound(0, 960), 960);
        assert_eq!(narrow_bound(1920, 960), 960);
        // A later 1600-wide request is compared to 1920, not previous 960.
        assert_eq!(narrow_bound(1920, 1600), 1600);
    }

    #[test]
    fn resize_then_restore_does_not_reauthorize_old_pixmap() {
        let mut state = grant();
        for width in [801, 800] {
            state.observe(&Event::ConfigureNotify(ConfigureNotifyEvent {
                window: 42,
                width,
                height: 600,
                ..Default::default()
            }));
        }
        assert!(state.check().is_ok());
        assert!(state.needs_rebind);
        assert_eq!(state.revision, 2);
        assert_eq!(state.observed_size, (800, 600));
    }

    #[test]
    fn resize_revision_overflow_revokes() {
        let mut state = grant();
        state.revision = u64::MAX;
        state.observe_size((801, 600));
        assert!(state.check().is_err());
    }

    #[test]
    fn border_change_then_restore_permanently_revokes() {
        let mut state = grant();
        for border_width in [1, 0] {
            state.observe(&Event::ConfigureNotify(ConfigureNotifyEvent {
                window: 42,
                width: 800,
                height: 600,
                border_width,
                ..Default::default()
            }));
        }
        assert!(state.check().is_err());
    }

    #[test]
    fn resize_does_not_override_lifecycle_revocation() {
        let mut state = grant();
        state.observe_size((801, 600));
        state.observe(&Event::DestroyNotify(DestroyNotifyEvent {
            window: 42,
            ..Default::default()
        }));
        state.observe_size((800, 600));
        assert!(state.needs_rebind);
        assert!(state.check().is_err());
    }

    #[test]
    fn revoke_at_any_point_in_a_capture_batch_remains_terminal() {
        for revoke_index in 0..8 {
            let mut state = grant();
            for index in 0..8 {
                if index == revoke_index {
                    state.observe(&Event::DestroyNotify(DestroyNotifyEvent {
                        window: 42,
                        ..Default::default()
                    }));
                } else {
                    state.observe(&Event::ConfigureNotify(ConfigureNotifyEvent {
                        window: 42,
                        width: 800,
                        height: 600,
                        ..Default::default()
                    }));
                }
            }
            // This also covers invalidation during pixmap setup/readback,
            // followed by benign notifications or an identically sized XID.
            assert!(state.check().is_err());
        }
    }
}
