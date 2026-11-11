//! Owned native wire + production capture. Capture ready is NOT viewer presentation.
#![cfg(feature = "backend-wayland-udev")]
use crate::backend::wayland_udev::{
    backend::fulfill_test_toplevel_capture,
    compositor::headless_render::HeadlessSmithayGles,
    image_copy_capture::{
        authority_tests::{App, Capture},
        init_image_copy_capture,
        wire_test_client::{Client, Event, Server},
    },
};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    os::fd::AsRawFd,
    time::{Duration, Instant},
};
const W: u32 = 32;
const H: u32 = 24;
fn fence(server: &mut Server, client: &mut Client) -> Vec<Event> {
    let done = client.new_id();
    client.request(1, 0, &[done]);
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut all = Vec::new();
    loop {
        server.roundtrip();
        let events = client.events();
        assert!(
            !events.iter().any(|e| e.sender == 1 && e.opcode == 0),
            "wl_display error"
        );
        let finished = events.iter().any(|e| e.sender == done && e.opcode == 0);
        all.extend(events);
        if finished {
            return all;
        }
        assert!(Instant::now() < deadline, "real display.sync timeout");
    }
}
fn global(event: &Event) -> Option<(u32, String, u32)> {
    if event.sender != 2 || event.opcode != 0 {
        return None;
    }
    let len = event.args[1] as usize;
    let bytes: Vec<_> = event.args[2..]
        .iter()
        .flat_map(|v| v.to_ne_bytes())
        .collect();
    Some((
        event.args[0],
        std::str::from_utf8(&bytes[..len - 1]).unwrap().into(),
        event.args[2 + len.div_ceil(4)],
    ))
}
fn bind_global(client: &mut Client, name: u32, interface: &str, version: u32) -> u32 {
    let id = client.new_id();
    let mut bytes = interface.as_bytes().to_vec();
    bytes.push(0);
    let len = bytes.len() as u32;
    bytes.resize(bytes.len().next_multiple_of(4), 0);
    let mut args = vec![name, len];
    args.extend(
        bytes
            .chunks_exact(4)
            .map(|b| u32::from_ne_bytes(b.try_into().unwrap())),
    );
    args.extend([version, id]);
    client.request(2, 0, &args);
    id
}
fn buffer(
    server: &mut Server,
    client: &mut Client,
    size: (u32, u32),
    pixel: [u8; 4],
) -> (u32, File) {
    assert!(size.0 > 0 && size.1 > 0 && size.0 <= W && size.1 <= H);
    let shm = client.bind("wl_shm", 1);
    let fd =
        nix::sys::memfd::memfd_create(c"remote-popup-test", nix::sys::memfd::MFdFlags::MFD_CLOEXEC)
            .unwrap();
    let len = size.0 * size.1 * 4;
    nix::unistd::ftruncate(&fd, i64::from(len)).unwrap();
    let mut file = File::from(fd);
    file.write_all(&pixel.repeat((len / 4) as usize)).unwrap();
    let pool = client.new_id();
    client.request_with_fd_args(shm, 0, &[pool, len], file.as_raw_fd());
    let id = client.new_id();
    client.request(pool, 0, &[id, 0, size.0, size.1, size.0 * 4, 0]);
    fence(server, client);
    (id, file)
}
struct Fixture {
    server: Server,
    app: App,
    other: App,
    capture: Capture,
    source: u32,
    lease: u32,
    epoch: u32,
    seat: u32,
    seat_global: u32,
    pointer: u32,
}
impl Fixture {
    fn new() -> Self {
        let mut server = Server::new();
        server.state.image_capture_pending =
            Some(init_image_copy_capture(&server.display.handle()));
        let mut app = App::with_size_color(&mut server, [0, 0, 255, 255], W, H);
        let mut capture = Capture::new_with_size(&mut server, (W, H));
        let mut other = App::with_size_color(&mut server, [0, 255, 255, 255], W, H);
        fence(&mut server, &mut app.client);
        fence(&mut server, &mut other.client);
        let manager = capture.client.bind("jwm_remote_popup_manager_v1", 1);
        let lease = capture.client.new_id();
        let source = capture.client.new_id();
        capture.client.request(manager, 1, &[lease, capture.source]);
        capture.client.request(lease, 1, &[source]);
        let events = fence(&mut server, &mut capture.client);
        let grant = events
            .iter()
            .find(|e| e.sender == lease && e.opcode == 0)
            .expect("real grant");
        assert_eq!(grant.args.len(), 2);
        assert_eq!(grant.args[1], 30_000);
        let epoch = grant.args[0];
        let globals = fence(&mut server, &mut app.client);
        let seats: Vec<_> = globals
            .iter()
            .filter_map(global)
            .filter(|g| g.1 == "wl_seat")
            .collect();
        assert_eq!(seats.len(), 1, "one newly advertised target-only seat");
        assert!(
            !fence(&mut server, &mut other.client)
                .iter()
                .filter_map(global)
                .any(|g| g.1 == "wl_seat"),
            "foreign client must not see the lease seat"
        );
        let seat = bind_global(&mut app.client, seats[0].0, "wl_seat", seats[0].2.min(7));
        let pointer = app.client.new_id();
        app.client.request(seat, 0, &[pointer]);
        fence(&mut server, &mut app.client);
        Self {
            server,
            app,
            other,
            capture,
            source,
            lease,
            epoch,
            seat,
            seat_global: seats[0].0,
            pointer,
        }
    }
    fn frame(&mut self, gl: &mut HeadlessSmithayGles) -> (u32, Vec<u8>) {
        let (buffer, mut file) = buffer(
            &mut self.server,
            &mut self.capture.client,
            (W, H),
            [0x5a; 4],
        );
        let session = self.capture.client.new_id();
        self.capture
            .client
            .request(self.capture.manager, 0, &[session, self.source, 0]);
        let constraints = fence(&mut self.server, &mut self.capture.client);
        assert!(
            constraints
                .iter()
                .any(|e| e.sender == session && e.opcode == 0 && e.args == [W, H])
        );
        let frame = self.capture.client.new_id();
        self.capture.client.request(session, 0, &[frame]);
        self.capture.client.request(frame, 1, &[buffer]);
        self.capture.client.request(frame, 3, &[]);
        fence(&mut self.server, &mut self.capture.client);
        let queue = self
            .server
            .state
            .image_capture_pending
            .as_ref()
            .unwrap()
            .clone();
        fulfill_test_toplevel_capture(&mut gl.renderer, &mut None, &self.server.state, &queue);
        let events = fence(&mut self.server, &mut self.capture.client);
        assert!(
            events.iter().any(|e| e.sender == frame && e.opcode == 3),
            "real GL capture ready"
        );
        assert!(!events.iter().any(|e| e.sender == frame && e.opcode == 4));
        file.seek(SeekFrom::Start(0)).unwrap();
        let mut pixels = vec![0; (W * H * 4) as usize];
        file.read_exact(&mut pixels).unwrap();
        (frame, pixels)
    }
    fn arm(&mut self, frame: u32) {
        self.capture
            .client
            .request(self.lease, 2, &[self.epoch, frame]);
        let events = fence(&mut self.server, &mut self.capture.client);
        assert!(
            events
                .iter()
                .any(|e| e.sender == self.lease && e.opcode == 1 && e.args == [self.epoch])
        );
        let events = fence(&mut self.server, &mut self.app.client);
        assert!(
            events.iter().any(|e| e.sender == self.pointer
                && e.opcode == 0
                && e.args[1] == self.app.surface)
        );
    }
    fn action(&mut self, frame: u32, seq: u32, kind: u32, x: u32, y: u32) -> Vec<Event> {
        self.capture
            .client
            .request(self.lease, 3, &[self.epoch, seq, frame, kind, x, y]);
        fence(&mut self.server, &mut self.capture.client)
    }
    fn accepted(&self, events: &[Event], seq: u32) {
        assert!(
            events
                .iter()
                .any(|e| e.sender == self.lease && e.opcode == 2 && e.args == [self.epoch, seq])
        );
    }
    fn revoked(&mut self, events: &[Event]) {
        assert!(
            events
                .iter()
                .any(|e| e.sender == self.lease && e.opcode == 3)
        );
        assert!(
            !events
                .iter()
                .any(|e| e.sender == self.lease && e.opcode == 2)
        );
        assert!(
            !fence(&mut self.server, &mut self.app.client)
                .iter()
                .any(|e| e.sender == self.pointer && matches!(e.opcode, 2..=4)),
            "invalid action delivered motion/button/axis"
        );
        assert!(
            !fence(&mut self.server, &mut self.other.client)
                .iter()
                .filter_map(global)
                .any(|g| g.1 == "wl_seat")
        );
    }
}
struct Popup {
    surface: u32,
    xdg: u32,
    role: u32,
}
fn popup(f: &mut Fixture, serial: u32) -> Popup {
    popup_with_grab(f, Some(serial))
}
fn popup_with_grab(f: &mut Fixture, serial: Option<u32>) -> Popup {
    let c = &mut f.app.client;
    let compositor = c.bind("wl_compositor", 6);
    let shell = c.bind("xdg_wm_base", 6);
    let pos = c.new_id();
    c.request(shell, 1, &[pos]);
    c.request(pos, 1, &[12, 10]);
    c.request(pos, 2, &[0, 0, 1, 1]);
    c.request(pos, 3, &[5]);
    c.request(pos, 4, &[8]);
    c.request(pos, 6, &[4, 3]);
    let surface = c.new_id();
    c.request(compositor, 0, &[surface]);
    let xdg = c.new_id();
    c.request(shell, 2, &[xdg, surface]);
    let role = c.new_id();
    c.request(xdg, 2, &[role, f.app.xdg, pos]);
    // Legal real client order: grab before initial mapping, with the serial
    // received on this exact independent seat's root button press.
    if let Some(serial) = serial {
        c.request(role, 1, &[f.seat, serial]);
    }
    c.request(surface, 6, &[]);
    let events = fence(&mut f.server, c);
    assert!(
        events
            .iter()
            .any(|e| e.sender == role && e.opcode == 0 && e.args == [4, 3, 12, 10])
    );
    let configure = events
        .iter()
        .find(|e| e.sender == xdg && e.opcode == 0)
        .expect("popup configure")
        .args[0];
    c.request(xdg, 4, &[configure]);
    let (buf, _file) = buffer(&mut f.server, c, (12, 10), [0, 255, 0, 255]);
    c.request(xdg, 3, &[0, 0, 12, 10]);
    c.request(surface, 1, &[buf, 0, 0]);
    c.request(surface, 6, &[]);
    fence(&mut f.server, c);
    Popup { surface, xdg, role }
}
#[test]
fn actual_popup_root_serial_direct_child_pixels_and_exact_recipient() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut f = Fixture::new();
    let local = f.server.state.seat.get_pointer().unwrap();
    let focus = local.current_focus();
    let location = local.current_location();
    let grabbed = local.is_grabbed();
    let pressed = local.has_pressed_buttons();
    let keyboard = f.server.state.seat.get_keyboard().unwrap();
    let keyboard_focus = keyboard.current_focus();
    let keyboard_grab = keyboard.is_grabbed();
    let (frame, pixels) = f.frame(&mut gl);
    assert!(pixels.chunks_exact(4).all(|p| p == [0, 0, 255, 255]));
    f.arm(frame);
    let (frame, _) = f.frame(&mut gl);
    let events = f.action(frame, 1, 1, 8, 8);
    f.accepted(&events, 1);
    let delivered = fence(&mut f.server, &mut f.app.client);
    let buttons: Vec<_> = delivered
        .iter()
        .filter(|e| e.sender == f.pointer && e.opcode == 3)
        .collect();
    assert_eq!(buttons.len(), 2);
    assert_eq!(&buttons[0].args[2..], &[0x110, 1]);
    assert_eq!(&buttons[1].args[2..], &[0x110, 0]);
    let serial = buttons[0].args[0];
    let popup = popup(&mut f, serial);
    let (frame, pixels) = f.frame(&mut gl);
    for y in 0..H {
        for x in 0..W {
            let expected = if (4..16).contains(&x) && (3..13).contains(&y) {
                [0, 255, 0, 255]
            } else {
                [0, 0, 255, 255]
            };
            assert_eq!(
                &pixels[((y * W + x) * 4) as usize..][..4],
                &expected,
                "pixel {x},{y}"
            );
        }
    }
    let events = f.action(frame, 2, 1, 7, 8);
    f.accepted(&events, 2);
    let delivered = fence(&mut f.server, &mut f.app.client);
    assert!(
        delivered
            .iter()
            .any(|e| e.sender == f.pointer && e.opcode == 0 && e.args[1] == popup.surface)
    );
    assert!(
        delivered
            .iter()
            .any(|e| e.sender == f.pointer && e.opcode == 2 && e.args[1..] == [3 * 256, 5 * 256])
    );
    let buttons: Vec<_> = delivered
        .iter()
        .filter(|e| e.sender == f.pointer && e.opcode == 3)
        .collect();
    assert_eq!(buttons.len(), 2);
    assert_eq!(&buttons[0].args[2..], &[0x110, 1]);
    assert_eq!(&buttons[1].args[2..], &[0x110, 0]);
    assert_eq!(local.current_focus(), focus);
    assert_eq!(local.current_location(), location);
    assert_eq!(local.is_grabbed(), grabbed);
    assert_eq!(local.has_pressed_buttons(), pressed);
    assert_eq!(keyboard.current_focus(), keyboard_focus);
    assert_eq!(keyboard.is_grabbed(), keyboard_grab);
}
#[test]
fn actual_popup_replayed_frame_and_unknown_action_never_deliver() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    for replay in [false, true] {
        let mut f = Fixture::new();
        let (frame, _) = f.frame(&mut gl);
        f.arm(frame);
        let (frame, _) = f.frame(&mut gl);
        if replay {
            let events = f.action(frame, 1, 0, 8, 8);
            f.accepted(&events, 1);
            fence(&mut f.server, &mut f.app.client);
        }
        let events = f.action(
            frame,
            if replay { 2 } else { 1 },
            if replay { 1 } else { 2 },
            8,
            8,
        );
        f.revoked(&events);
    }
}

#[test]
fn actual_popup_root_unmap_and_lock_aba_cannot_restore_old_proof() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    for lock in [false, true] {
        let mut f = Fixture::new();
        let (frame, _) = f.frame(&mut gl);
        f.arm(frame);
        let (frame, _) = f.frame(&mut gl);
        if lock {
            f.server.state.session_lock_supported = true;
            let mut locker = f.server.connect();
            let manager = locker.bind("ext_session_lock_manager_v1", 1);
            let resource = locker.new_id();
            locker.request(manager, 1, &[resource]);
            fence(&mut f.server, &mut locker);
            assert!(f.server.state.session_locked);
            locker.request(resource, 2, &[]);
            fence(&mut f.server, &mut locker);
            assert!(!f.server.state.session_locked);
        } else {
            f.app.unmap(&mut f.server);
            f.app.remap(&mut f.server);
        }
        let events = f.action(frame, 1, 1, 8, 8);
        f.revoked(&events);
    }
}

#[test]
fn actual_popup_original_readonly_frame_cannot_replace_lease_family_proof() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut f = Fixture::new();
    let (frame, _) = f.frame(&mut gl);
    f.arm(frame);
    // Same original target and dimensions are insufficient: this successful
    // frame comes from the original read-only source, not the lease family.
    let family = f.source;
    f.source = f.capture.source;
    let (foreign_scope, _) = f.frame(&mut gl);
    f.source = family;
    let events = f.action(foreign_scope, 1, 1, 8, 8);
    f.revoked(&events);
}

fn root_click(f: &mut Fixture, gl: &mut HeadlessSmithayGles) -> u32 {
    let (frame, _) = f.frame(gl);
    f.arm(frame);
    let (frame, _) = f.frame(gl);
    let events = f.action(frame, 1, 1, 8, 8);
    f.accepted(&events, 1);
    let events = fence(&mut f.server, &mut f.app.client);
    let buttons: Vec<_> = events
        .iter()
        .filter(|e| e.sender == f.pointer && e.opcode == 3)
        .collect();
    assert_eq!(buttons.len(), 2);
    assert_eq!(&buttons[0].args[2..], &[0x110, 1]);
    assert_eq!(&buttons[1].args[2..], &[0x110, 0]);
    buttons[0].args[0]
}
#[test]
fn actual_popup_mapping_requires_restricted_grab_and_unmap_is_terminal() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    for without_grab in [true, false] {
        let mut f = Fixture::new();
        let serial = root_click(&mut f, &mut gl);
        if without_grab {
            popup_with_grab(&mut f, None);
            let events = fence(&mut f.server, &mut f.capture.client);
            f.revoked(&events);
        } else {
            let popup = popup(&mut f, serial);
            let (frame, _) = f.frame(&mut gl);
            f.app.client.request(popup.surface, 1, &[0, 0, 0]);
            f.app.client.request(popup.surface, 6, &[]);
            fence(&mut f.server, &mut f.app.client);
            // Attach the same dimensions again: the lease cannot fall back to
            // root targeting or accept the pre-unmap family snapshot.
            let (buf, _file) = buffer(&mut f.server, &mut f.app.client, (12, 10), [0, 255, 0, 255]);
            f.app.client.request(popup.surface, 1, &[buf, 0, 0]);
            f.app.client.request(popup.surface, 6, &[]);
            fence(&mut f.server, &mut f.app.client);
            let events = f.action(frame, 2, 1, 7, 8);
            f.revoked(&events);
        }
    }
}
fn create_role(client: &mut Client, parent: u32) -> (u32, u32, u32) {
    let compositor = client.bind("wl_compositor", 6);
    let shell = client.bind("xdg_wm_base", 6);
    let pos = client.new_id();
    client.request(shell, 1, &[pos]);
    client.request(pos, 1, &[4, 4]);
    client.request(pos, 2, &[0, 0, 1, 1]);
    let surface = client.new_id();
    client.request(compositor, 0, &[surface]);
    let xdg = client.new_id();
    client.request(shell, 2, &[xdg, surface]);
    let role = client.new_id();
    client.request(xdg, 2, &[role, parent, pos]);
    (surface, xdg, role)
}
#[test]
fn actual_popup_nested_and_foreign_parent_cannot_join_authority() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    for foreign in [false, true] {
        let mut f = Fixture::new();
        let serial = root_click(&mut f, &mut gl);
        if foreign {
            let parent = f.other.xdg;
            create_role(&mut f.other.client, parent);
            fence(&mut f.server, &mut f.other.client);
        } else {
            let popup = popup(&mut f, serial);
            create_role(&mut f.app.client, popup.xdg);
            fence(&mut f.server, &mut f.app.client);
        }
        let events = fence(&mut f.server, &mut f.capture.client);
        f.revoked(&events);
    }
}
#[test]
fn actual_popup_reusing_consumed_grab_serial_revokes() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut f = Fixture::new();
    let serial = root_click(&mut f, &mut gl);
    let popup = popup(&mut f, serial);
    f.app.client.request(popup.role, 1, &[f.seat, serial]);
    f.app.client.request(popup.surface, 6, &[]);
    fence(&mut f.server, &mut f.app.client);
    let events = fence(&mut f.server, &mut f.capture.client);
    f.revoked(&events);
}

#[test]
fn actual_popup_geometry_change_and_restore_rejects_old_family_frame() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut f = Fixture::new();
    let serial = root_click(&mut f, &mut gl);
    let popup = popup(&mut f, serial);
    let (old_frame, _) = f.frame(&mut gl);
    f.app.client.request(popup.xdg, 3, &[1, 0, 11, 10]);
    f.app.client.request(popup.surface, 6, &[]);
    fence(&mut f.server, &mut f.app.client);
    f.app.client.request(popup.xdg, 3, &[0, 0, 12, 10]);
    f.app.client.request(popup.surface, 6, &[]);
    fence(&mut f.server, &mut f.app.client);
    let events = f.action(old_frame, 2, 1, 7, 8);
    f.revoked(&events);
}

#[test]
fn actual_popup_original_thirty_second_timer_retires_seat_without_input() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut f = Fixture::new();
    let timing = super::lease_timing_for_test(&f.server.state).expect("actual created lease");
    assert_eq!(timing.0, f.epoch);
    assert_eq!(timing.2.duration_since(timing.1), Duration::from_secs(30));
    let local = f.server.state.seat.get_pointer().unwrap();
    let focus = local.current_focus();
    let location = local.current_location();
    let held = local.has_pressed_buttons();
    let grabbed = local.is_grabbed();
    let (frame, _) = f.frame(&mut gl);
    f.arm(frame);
    let end = timing.2 + Duration::from_secs(2);
    let mut revoked = false;
    let mut removed = false;
    loop {
        assert!(
            Instant::now() < end,
            "real timer/seat retirement did not finish"
        );
        f.server.dispatch_timers(Duration::from_millis(10));
        let lease_events = fence(&mut f.server, &mut f.capture.client);
        for event in lease_events
            .iter()
            .filter(|e| e.sender == f.lease && e.opcode == 3)
        {
            assert!(
                Instant::now() >= timing.2,
                "lease revoked before original deadline"
            );
            assert_eq!(event.args, [5], "must be actual EXPIRED reason");
            assert!(!revoked, "duplicate revoked");
            revoked = true;
        }
        let app_events = fence(&mut f.server, &mut f.app.client);
        assert!(
            !app_events
                .iter()
                .any(|e| e.sender == f.pointer && matches!(e.opcode, 2..=4)),
            "expiry cannot synthesize input"
        );
        for event in app_events
            .iter()
            .filter(|e| e.sender == 2 && e.opcode == 1 && e.args == [f.seat_global])
        {
            assert_eq!(event.args, [f.seat_global]);
            assert!(Instant::now() >= timing.2);
            assert!(!removed);
            removed = true;
        }
        if let Some(now) = super::lease_timing_for_test(&f.server.state) {
            assert_eq!(now, timing, "expiry must never renew");
        }
        if revoked && removed {
            break;
        }
    }
    f.capture
        .client
        .request(f.lease, 3, &[f.epoch, 1, frame, 1, 8, 8]);
    let events = fence(&mut f.server, &mut f.capture.client);
    assert!(
        !events
            .iter()
            .any(|e| e.sender == f.lease && matches!(e.opcode, 0..=2))
    );
    assert!(
        !fence(&mut f.server, &mut f.app.client)
            .iter()
            .any(|e| e.sender == f.pointer && matches!(e.opcode, 2..=4))
    );
    assert_eq!(local.current_focus(), focus);
    assert_eq!(local.current_location(), location);
    assert_eq!(local.has_pressed_buttons(), held);
    assert_eq!(local.is_grabbed(), grabbed);
    assert!(super::lease_timing_for_test(&f.server.state).is_none());
}

#[test]
fn actual_popup_serial_cannot_start_move_resize_or_drag() {
    use crate::backend::api::BackendEvent;
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut f = Fixture::new();
    let serial = root_click(&mut f, &mut gl);
    let private_pointer = f
        .server
        .state
        .remote_popup
        .active
        .as_ref()
        .unwrap()
        .seat
        .get_pointer()
        .unwrap();
    let local = f.server.state.seat.get_pointer().unwrap();
    let local_focus = local.current_focus();
    let local_at = local.current_location();
    let local_grab = local.is_grabbed();
    let requests = || {
        f.server
            .backend_events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| matches!(e, BackendEvent::MoveResizeRequest { .. }))
            .count()
    };
    let before = requests();
    f.app.client.request(f.app.toplevel, 5, &[f.seat, serial]);
    f.app
        .client
        .request(f.app.toplevel, 6, &[f.seat, serial, 8]);
    let manager = f.app.client.bind("wl_data_device_manager", 3);
    let device = f.app.client.new_id();
    f.app.client.request(manager, 1, &[device, f.seat]);
    fence(&mut f.server, &mut f.app.client);
    // Null source is a valid same-client drag request. A real implicit grab
    // would authorize it, but the restricted click must never create that grab.
    f.app
        .client
        .request(device, 0, &[0, f.app.surface, 0, serial]);
    let events = fence(&mut f.server, &mut f.app.client);
    assert!(
        !events
            .iter()
            .any(|e| e.sender == device && matches!(e.opcode, 1..=4)),
        "restricted serial started DnD"
    );
    assert_eq!(
        f.server
            .backend_events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| matches!(e, BackendEvent::MoveResizeRequest { .. }))
            .count(),
        before
    );
    assert!(!private_pointer.is_grabbed());
    assert!(!private_pointer.has_pressed_buttons());
    assert_eq!(local.current_focus(), local_focus);
    assert_eq!(local.current_location(), local_at);
    assert_eq!(local.is_grabbed(), local_grab);
    // No-effect denial is not required to revoke the popup lease. Its one
    // legitimate purpose remains usable by the exact direct child.
    popup(&mut f, serial);
    let (frame, _) = f.frame(&mut gl);
    let events = f.action(frame, 2, 1, 7, 8);
    f.accepted(&events, 2);
}

#[test]
fn actual_popup_pending_unmapped_child_invalidates_root_only_proof() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut f = Fixture::new();
    root_click(&mut f, &mut gl);
    let (root_frame, _) = f.frame(&mut gl);
    let parent = f.app.xdg;
    create_role(&mut f.app.client, parent);
    fence(&mut f.server, &mut f.app.client);
    // No buffer or grab exists for the newly created child. It must not be
    // treated as absence of a popup and fall back to the old root proof.
    let events = f.action(root_frame, 2, 1, 8, 8);
    f.revoked(&events);
}

#[path = "remote_popup_exclusivity_tests.rs"]
mod exclusivity;

#[test]
fn actual_popup_owner_duplicate_source_export_permanently_revokes() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut f = Fixture::new();
    let (frame, _) = f.frame(&mut gl);
    f.arm(frame);
    let (old_frame, _) = f.frame(&mut gl);
    let duplicate = f.capture.client.new_id();
    f.capture.client.request(f.lease, 1, &[duplicate]);
    let events = fence(&mut f.server, &mut f.capture.client);
    f.revoked(&events);
    assert!(super::lease_timing_for_test(&f.server.state).is_none());
    let session = f.capture.client.new_id();
    f.capture
        .client
        .request(f.capture.manager, 0, &[session, duplicate, 0]);
    let events = fence(&mut f.server, &mut f.capture.client);
    assert!(
        events.iter().any(|e| e.sender == session && e.opcode == 5),
        "duplicate export is an inert source"
    );
    assert!(
        !events
            .iter()
            .any(|e| e.sender == session && matches!(e.opcode, 0 | 4)),
        "inert export cannot advertise usable constraints"
    );
    f.capture
        .client
        .request(f.lease, 3, &[f.epoch, 1, old_frame, 1, 8, 8]);
    let events = fence(&mut f.server, &mut f.capture.client);
    assert!(
        !events
            .iter()
            .any(|e| e.sender == f.lease && matches!(e.opcode, 0..=2))
    );
    assert!(
        !fence(&mut f.server, &mut f.app.client)
            .iter()
            .any(|e| e.sender == f.pointer && matches!(e.opcode, 2..=4)),
        "revoked owner cannot use its original successful proof"
    );
}
