//! Real Wayland wire dispatch regressions. These tests use owned socketpairs,
//! real xdg roles and wl_shm buffers. The offscreen test also uses the actual
//! production GlesRenderer capture path; no test exercises DRM presentation.
//! Run in CI with socket permission; compilation alone is not execution proof.
use super::wire_test_client::{Client, Server};
use super::*;
use std::fs::File;
use std::io::Write;
#[cfg(feature = "backend-wayland-udev")]
use std::io::{Read, Seek, SeekFrom};
use std::os::fd::AsRawFd;

const W: u32 = 800;
const H: u32 = 600;
const SESSION_STOPPED: u16 = 5;
const FRAME_FAILED: u16 = 4;
const FAILURE_STOPPED: u32 = 2;

fn roundtrip(server: &mut Server) {
    // A received SCM_RIGHTS boundary may end one dispatch before attach/commit.
    server.roundtrip();
    server.roundtrip();
}

fn no_protocol_error(client: &mut Client) -> Vec<wire_test_client::Event> {
    let events = client.events();
    assert!(
        events.iter().all(|e| !(e.sender == 1 && e.opcode == 0)),
        "unexpected wl_display.error"
    );
    events
}

fn shm_buffer(server: &mut Server, client: &mut Client) -> u32 {
    shm_buffer_file(server, client, [0, 0, 255, 255]).0
}

pub(crate) fn shm_buffer_file(
    server: &mut Server,
    client: &mut Client,
    pixel: [u8; 4],
) -> (u32, File) {
    sized_shm_buffer_file(server, client, pixel, W, H)
}

fn sized_shm_buffer_file(
    server: &mut Server,
    client: &mut Client,
    pixel: [u8; 4],
    width: u32,
    height: u32,
) -> (u32, File) {
    assert!(width > 0 && height > 0 && width <= W && height <= H);
    let shm = client.bind("wl_shm", 1);
    let fd = nix::sys::memfd::memfd_create(
        c"capture-authority-wire",
        nix::sys::memfd::MFdFlags::MFD_CLOEXEC,
    )
    .unwrap();
    nix::unistd::ftruncate(&fd, i64::from(width * height * 4)).unwrap();
    let mut file = File::from(fd);
    file.write_all(&pixel.repeat((width * height) as usize))
        .unwrap();
    let pool = client.new_id();
    client.request_with_fd_args(shm, 0, &[pool, width * height * 4], file.as_raw_fd());
    let buffer = client.new_id();
    client.request(pool, 0, &[buffer, 0, width, height, width * 4, 0]); // ARGB8888
    roundtrip(server);
    no_protocol_error(client);
    (buffer, file)
}

pub(crate) struct App {
    pub(crate) client: Client,
    pub(crate) surface: u32,
    pub(crate) xdg: u32,
    pub(crate) toplevel: u32,
    buffer: u32,
    pub(crate) window: WindowId,
}
impl App {
    pub(crate) fn new(server: &mut Server) -> Self {
        Self::with_color(server, [0, 0, 255, 255])
    }

    pub(crate) fn with_color(server: &mut Server, color: [u8; 4]) -> Self {
        Self::with_size_color(server, color, W, H)
    }

    pub(crate) fn with_size_color(
        server: &mut Server,
        color: [u8; 4],
        width: u32,
        height: u32,
    ) -> Self {
        let previous: std::collections::HashSet<_> =
            server.state.toplevels.keys().copied().collect();
        let mut client = server.connect();
        let compositor = client.bind("wl_compositor", 6);
        let shell = client.bind("xdg_wm_base", 6);
        let surface = client.new_id();
        client.request(compositor, 0, &[surface]);
        let xdg = client.new_id();
        client.request(shell, 2, &[xdg, surface]);
        let toplevel = client.new_id();
        client.request(xdg, 1, &[toplevel]);
        client.request(surface, 6, &[]);
        roundtrip(server);
        let window = *server
            .state
            .toplevels
            .keys()
            .find(|key| !previous.contains(key))
            .expect("new real xdg toplevel");
        // The headless wire harness has no WM layout tick. Set its layout
        // dimensions before the real configure/ACK and matching SHM commit.
        if (width, height) != (W, H) {
            let geometry = server.state.window_geometry.get_mut(&window).unwrap();
            geometry.w = width;
            geometry.h = height;
            server.state.toplevels[&window].with_pending_state(|state| {
                state.size = Some((width as i32, height as i32).into());
            });
        }
        server.state.toplevels[&window].send_configure();
        roundtrip(server);
        let serial = no_protocol_error(&mut client)
            .into_iter()
            .filter(|e| e.sender == xdg && e.opcode == 0)
            .next_back()
            .expect("xdg configure")
            .args[0];
        client.request(xdg, 4, &[serial]);
        let (buffer, _file) = sized_shm_buffer_file(server, &mut client, color, width, height);
        client.request(surface, 1, &[buffer, 0, 0]);
        client.request(surface, 6, &[]);
        roundtrip(server);
        no_protocol_error(&mut client);
        assert!(server.state.mapped_windows.contains(&window));
        Self {
            client,
            surface,
            xdg,
            toplevel,
            buffer,
            window,
        }
    }

    pub(crate) fn unmap(&mut self, server: &mut Server) {
        self.client.request(self.surface, 1, &[0, 0, 0]);
        self.client.request(self.surface, 6, &[]);
        roundtrip(server);
        no_protocol_error(&mut self.client);
        assert!(!server.state.mapped_windows.contains(&self.window));
    }

    pub(crate) fn remap(&mut self, server: &mut Server) {
        // xdg-shell requires another configure after unmapping.
        self.client.request(self.surface, 6, &[]);
        roundtrip(server);
        server.state.toplevels[&self.window].send_configure();
        roundtrip(server);
        let serial = no_protocol_error(&mut self.client)
            .into_iter()
            .filter(|e| e.sender == self.xdg && e.opcode == 0)
            .next_back()
            .expect("remap configure")
            .args[0];
        self.client.request(self.xdg, 4, &[serial]);
        self.client.request(self.surface, 1, &[self.buffer, 0, 0]);
        self.client.request(self.surface, 6, &[]);
        roundtrip(server);
        no_protocol_error(&mut self.client);
        assert!(server.state.mapped_windows.contains(&self.window));
    }
}

pub(crate) struct Capture {
    pub(crate) client: Client,
    pub(crate) manager: u32,
    pub(crate) sources: u32,
    pub(crate) handle: u32,
    pub(crate) source: u32,
    pub(crate) session: u32,
}
impl Capture {
    pub(crate) fn new(server: &mut Server) -> Self {
        Self::new_with_size(server, (W, H))
    }

    pub(crate) fn new_with_size(server: &mut Server, size: (u32, u32)) -> Self {
        let mut client = server.connect();
        let list = client.bind("ext_foreign_toplevel_list_v1", 1);
        let sources = client.bind("ext_foreign_toplevel_image_capture_source_manager_v1", 1);
        let manager = client.bind("ext_image_copy_capture_manager_v1", 1);
        roundtrip(server);
        let handle = no_protocol_error(&mut client)
            .into_iter()
            .find(|e| e.sender == list && e.opcode == 0)
            .expect("real foreign handle")
            .args[0];
        let source = client.new_id();
        client.request(sources, 0, &[source, handle]);
        let session = client.new_id();
        client.request(manager, 0, &[session, source, 0]);
        roundtrip(server);
        let events = no_protocol_error(&mut client);
        assert!(
            events
                .iter()
                .any(|e| e.sender == session && e.opcode == 0 && e.args == [size.0, size.1]),
            "positive control: live source buffer size"
        );
        assert!(
            events.iter().any(|e| e.sender == session && e.opcode == 4),
            "positive control: live constraints done"
        );
        Self {
            client,
            manager,
            sources,
            handle,
            source,
            session,
        }
    }

    fn assert_stopped_capture(&mut self, server: &mut Server) {
        let frame = self.client.new_id();
        self.client.request(self.session, 0, &[frame]);
        self.client.request(frame, 3, &[]);
        roundtrip(server);
        let events = no_protocol_error(&mut self.client);
        assert!(
            events.iter().any(|e| e.sender == frame
                && e.opcode == FRAME_FAILED
                && e.args == [FAILURE_STOPPED])
        );
        assert!(
            !events.iter().any(|e| e.sender == frame && e.opcode == 3),
            "revoked frame must never be ready"
        );
        self.client.request(frame, 0, &[]);
        roundtrip(server);
    }

    fn assert_old_source_cannot_reauthorize(&mut self, server: &mut Server) {
        let session = self.client.new_id();
        self.client
            .request(self.manager, 0, &[session, self.source, 0]);
        roundtrip(server);
        let events = no_protocol_error(&mut self.client);
        assert!(
            events
                .iter()
                .any(|e| e.sender == session && e.opcode == SESSION_STOPPED)
        );
        assert!(!events.iter().any(|e| e.sender == session && e.opcode == 0));
    }
}

fn server() -> Server {
    let mut server = Server::new();
    server.state.image_capture_pending = Some(init_image_copy_capture(&server.display.handle()));
    server
}

#[test]
fn wire_null_buffer_unmap_then_remap_cannot_revive_source() {
    let mut server = server();
    let mut app = App::new(&mut server);
    let mut capture = Capture::new(&mut server);
    app.unmap(&mut server);
    app.remap(&mut server);
    capture.assert_stopped_capture(&mut server);
    capture.assert_old_source_cannot_reauthorize(&mut server);
    // A fresh locally selected source can authorize the newly mapped lifetime.
    let _fresh = Capture::new(&mut server);
}

#[test]
fn wire_lock_unlock_cannot_revive_selected_surface() {
    let mut server = server();
    let _app = App::new(&mut server);
    let mut capture = Capture::new(&mut server);
    server.state.session_lock_supported = true;
    let mut locker = server.connect();
    let manager = locker.bind("ext_session_lock_manager_v1", 1);
    let lock = locker.new_id();
    locker.request(manager, 1, &[lock]);
    roundtrip(&mut server);
    assert!(server.state.session_locked);
    assert!(
        no_protocol_error(&mut locker)
            .iter()
            .any(|e| e.sender == lock && e.opcode == 0)
    );
    capture.assert_stopped_capture(&mut server);
    locker.request(lock, 2, &[]);
    roundtrip(&mut server);
    assert!(!server.state.session_locked);
    capture.assert_stopped_capture(&mut server);
    capture.assert_old_source_cannot_reauthorize(&mut server);
}

#[test]
fn wire_role_destroyed_cannot_reuse_old_source() {
    let mut server = server();
    let mut app = App::new(&mut server);
    let mut capture = Capture::new(&mut server);
    app.client.request(app.toplevel, 0, &[]);
    roundtrip(&mut server);
    no_protocol_error(&mut app.client);
    assert!(!server.state.window_geometry.contains_key(&app.window));
    capture.assert_stopped_capture(&mut server);
    capture.assert_old_source_cannot_reauthorize(&mut server);
}

fn queued_frame(server: &mut Server, capture: &mut Capture) -> PendingImageCapture {
    let buffer = shm_buffer(server, &mut capture.client);
    let frame = capture.client.new_id();
    capture.client.request(capture.session, 0, &[frame]);
    capture.client.request(frame, 1, &[buffer]);
    capture.client.request(frame, 3, &[]);
    roundtrip(server);
    no_protocol_error(&mut capture.client);
    let pending = server
        .state
        .image_capture_pending
        .as_ref()
        .unwrap()
        .lock()
        .unwrap()
        .pop()
        .expect("positive control: real capture queued");
    assert!(
        pending.authorized(&server.state),
        "live queued capture is authorized"
    );
    pending
}

#[test]
fn wire_pending_frame_revoked_by_lock_before_renderer_write() {
    let mut server = server();
    let _app = App::new(&mut server);
    let mut capture = Capture::new(&mut server);
    let pending = queued_frame(&mut server, &mut capture);
    server.state.session_lock_supported = true;
    let mut locker = server.connect();
    let manager = locker.bind("ext_session_lock_manager_v1", 1);
    let lock = locker.new_id();
    locker.request(manager, 1, &[lock]);
    roundtrip(&mut server);
    assert!(server.state.session_locked);
    assert!(
        !pending.authorized(&server.state),
        "renderer must reject already dequeued frame after lock"
    );
    assert!(
        no_protocol_error(&mut locker)
            .iter()
            .any(|e| e.sender == lock && e.opcode == 0)
    );
    locker.request(lock, 2, &[]);
    roundtrip(&mut server);
    assert!(!server.state.session_locked);
    assert!(
        !pending.authorized(&server.state),
        "unlock cannot restore a pending frame's old grant"
    );
}

#[test]
fn wire_pending_frame_revoked_by_capture_client_disconnect() {
    let mut server = server();
    let _app = App::new(&mut server);
    let mut capture = Capture::new(&mut server);
    let pending = queued_frame(&mut server, &mut capture);
    drop(capture);
    roundtrip(&mut server);
    assert!(
        !pending.authorized(&server.state),
        "disconnected session cannot deliver held frame"
    );
}

#[test]
fn wire_pending_frame_revoked_by_source_client_disconnect() {
    let mut server = server();
    let app = App::new(&mut server);
    let mut capture = Capture::new(&mut server);
    let pending = queued_frame(&mut server, &mut capture);
    drop(app);
    roundtrip(&mut server);
    assert!(
        !pending.authorized(&server.state),
        "closed source cannot deliver held frame"
    );
    capture.assert_old_source_cannot_reauthorize(&mut server);
}

#[test]
fn wire_pending_frame_revoked_by_unmap_remap_before_renderer_write() {
    let mut server = server();
    let mut app = App::new(&mut server);
    let mut capture = Capture::new(&mut server);
    let pending = queued_frame(&mut server, &mut capture);
    app.unmap(&mut server);
    app.remap(&mut server);
    assert!(
        !pending.authorized(&server.state),
        "mapped-again surface cannot restore old queued authority"
    );
}

#[cfg(feature = "backend-wayland-udev")]
#[test]
fn wire_real_offscreen_pixels_isolate_surface_and_revocation_prevents_write() {
    use crate::backend::wayland_udev::backend::fulfill_test_toplevel_capture;
    use crate::backend::wayland_udev::compositor::headless_render::HeadlessSmithayGles;

    let Some(mut gl) = HeadlessSmithayGles::new() else {
        // CI sets JWM_REQUIRE_HEADLESS_GL=1; unavailable GL must fail there.
        return;
    };
    for revoke_by_lock in [false, true] {
        let mut server = server();
        let mut red_app = App::with_color(&mut server, [0, 0, 255, 255]);
        // Select the sole toplevel BEFORE adding the unrelated blue surface.
        let mut capture = Capture::new(&mut server);
        let _blue_app = App::with_color(&mut server, [255, 0, 0, 255]);
        let mut cache = None;
        let queue = server.state.image_capture_pending.as_ref().unwrap().clone();
        let queue_output = |server: &mut Server, capture: &mut Capture| {
            let (buffer, file) = shm_buffer_file(server, &mut capture.client, [0x5a; 4]);
            let frame = capture.client.new_id();
            capture.client.request(capture.session, 0, &[frame]);
            capture.client.request(frame, 1, &[buffer]);
            capture.client.request(frame, 3, &[]);
            roundtrip(server);
            no_protocol_error(&mut capture.client);
            assert_eq!(queue.lock().unwrap().len(), 1);
            (frame, file)
        };
        let (frame, mut file) = queue_output(&mut server, &mut capture);
        fulfill_test_toplevel_capture(&mut gl.renderer, &mut cache, &server.state, &queue);
        roundtrip(&mut server);
        let events = no_protocol_error(&mut capture.client);
        assert!(
            events.iter().any(|e| e.sender == frame && e.opcode == 3),
            "production renderer must send ready"
        );
        file.seek(SeekFrom::Start(0)).unwrap();
        let mut pixels = Vec::new();
        file.read_to_end(&mut pixels).unwrap();
        assert_eq!(pixels.len(), (W * H * 4) as usize);
        assert!(
            pixels
                .chunks_exact(4)
                .all(|pixel| pixel == [0, 0, 255, 255]),
            "single-surface capture must contain only selected red pixels, not overlapping blue/desktop"
        );
        capture.client.request(frame, 0, &[]);
        roundtrip(&mut server);
        let (revoked_frame, mut untouched) = queue_output(&mut server, &mut capture);
        let mut locker = None;
        if revoke_by_lock {
            server.state.session_lock_supported = true;
            let mut client = server.connect();
            let manager = client.bind("ext_session_lock_manager_v1", 1);
            let lock = client.new_id();
            client.request(manager, 1, &[lock]);
            roundtrip(&mut server);
            assert!(server.state.session_locked);
            locker = Some(client);
        } else {
            red_app.unmap(&mut server);
            red_app.remap(&mut server);
        }
        fulfill_test_toplevel_capture(&mut gl.renderer, &mut cache, &server.state, &queue);
        roundtrip(&mut server);
        let events = no_protocol_error(&mut capture.client);
        assert!(
            !events
                .iter()
                .any(|e| e.sender == revoked_frame && e.opcode == 3)
        );
        untouched.seek(SeekFrom::Start(0)).unwrap();
        let mut bytes = Vec::new();
        untouched.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes.len(), (W * H * 4) as usize);
        assert!(
            bytes.iter().all(|byte| *byte == 0x5a),
            "revoked capture must not write destination SHM"
        );
        drop(locker);
    }
}

#[test]
fn wire_source_quota_stops_only_the_excess_source_session() {
    let mut server = server();
    let _app = App::new(&mut server);
    let mut capture = Capture::new(&mut server);
    for _ in 1..MAX_TOPLEVEL_SOURCES {
        let source = capture.client.new_id();
        capture
            .client
            .request(capture.sources, 0, &[source, capture.handle]);
    }
    roundtrip(&mut server);
    no_protocol_error(&mut capture.client);
    let excess_source = capture.client.new_id();
    capture
        .client
        .request(capture.sources, 0, &[excess_source, capture.handle]);
    let excess_session = capture.client.new_id();
    capture
        .client
        .request(capture.manager, 0, &[excess_session, excess_source, 0]);
    roundtrip(&mut server);
    let events = no_protocol_error(&mut capture.client);
    let stopped: Vec<_> = events
        .iter()
        .filter(|e| e.opcode == SESSION_STOPPED)
        .map(|e| e.sender)
        .collect();
    assert_eq!(
        stopped,
        [excess_session],
        "quota failure belongs only to excess capture session"
    );
    assert!(
        !events
            .iter()
            .any(|e| e.sender == excess_session && e.opcode == 0)
    );
    let pending = queued_frame(&mut server, &mut capture);
    assert!(
        pending.authorized(&server.state),
        "source quota must not revoke an existing live session"
    );
}

#[test]
fn wire_session_quota_stops_only_excess_not_existing_session() {
    let mut server = server();
    let _app = App::new(&mut server);
    let mut capture = Capture::new(&mut server);
    for _ in 1..MAX_TOPLEVEL_SESSIONS {
        let session = capture.client.new_id();
        capture
            .client
            .request(capture.manager, 0, &[session, capture.source, 0]);
        // Bound the wire test itself and avoid depending on socket buffer size.
        roundtrip(&mut server);
        let events = no_protocol_error(&mut capture.client);
        assert!(events.iter().any(|e| e.sender == session && e.opcode == 4));
        assert!(!events.iter().any(|e| e.opcode == SESSION_STOPPED));
    }
    let excess_session = capture.client.new_id();
    capture
        .client
        .request(capture.manager, 0, &[excess_session, capture.source, 0]);
    roundtrip(&mut server);
    let events = no_protocol_error(&mut capture.client);
    let stopped: Vec<_> = events
        .iter()
        .filter(|e| e.opcode == SESSION_STOPPED)
        .map(|e| e.sender)
        .collect();
    assert_eq!(
        stopped,
        [excess_session],
        "session quota error is scoped, not a global stop"
    );
    assert!(
        !events
            .iter()
            .any(|e| e.sender == excess_session && e.opcode == 0)
    );
    assert!(queued_frame(&mut server, &mut capture).authorized(&server.state));
}

#[test]
fn wire_buffer_scale_or_transform_change_restore_cannot_revive_old_source() {
    // wl_surface.set_buffer_transform opcode 7; set_buffer_scale opcode 8.
    for (opcode, changed, original) in [(7, 2, 0), (8, 2, 1)] {
        let mut server = server();
        let mut app = App::new(&mut server);
        let mut capture = Capture::new(&mut server);
        let pending = queued_frame(&mut server, &mut capture);
        for value in [changed, original] {
            app.client.request(app.surface, opcode, &[value]);
            app.client.request(app.surface, 6, &[]);
            roundtrip(&mut server);
            no_protocol_error(&mut app.client);
        }
        assert!(
            !pending.authorized(&server.state),
            "buffer layout history cannot be erased by restoring the old value"
        );
        capture.assert_old_source_cannot_reauthorize(&mut server);
    }
}
