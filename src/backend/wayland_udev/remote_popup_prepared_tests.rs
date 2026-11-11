//! Real lifecycle tests for trusted preparation; GTK startup ordering is tested
//! separately by the controlled-launch subprocess probe. No synthetic clocks.
use super::*;
use crate::backend::wayland::state::JwmWaylandState;
use smithay::{input::Seat, reexports::wayland_server::Resource};

struct Prepared {
    server: Server,
    app: App,
    capture: Capture,
    seat: Seat<JwmWaylandState>,
    global: u32,
    seat_id: u32,
    pointer: u32,
}
impl Prepared {
    fn new() -> Self {
        let mut server = Server::new();
        server.state.image_capture_pending =
            Some(init_image_copy_capture(&server.display.handle()));
        let mut app = App::with_size_color(&mut server, [0, 0, 255, 255], W, H);
        let capture = Capture::new_with_size(&mut server, (W, H));
        fence(&mut server, &mut app.client);
        let owner = server
            .state
            .surface_for_window(app.window)
            .unwrap()
            .client()
            .unwrap()
            .id();
        let seat = super::super::prepare_dormant(&mut server.state, owner).unwrap();
        let events = fence(&mut server, &mut app.client);
        let seats: Vec<_> = events
            .iter()
            .filter_map(global)
            .filter(|g| g.1 == "wl_seat")
            .collect();
        assert_eq!(seats.len(), 1);
        let seat_id = bind_global(&mut app.client, seats[0].0, "wl_seat", 7);
        let pointer = app.client.new_id();
        app.client.request(seat_id, 0, &[pointer]);
        let events = fence(&mut server, &mut app.client);
        assert!(!events.iter().any(|e| e.sender == pointer));
        assert!(seat.get_keyboard().is_none());
        assert!(!super::super::active(&server.state));
        Self {
            server,
            app,
            capture,
            seat,
            global: seats[0].0,
            seat_id,
            pointer,
        }
    }
    fn grant(mut self) -> (Fixture, Seat<JwmWaylandState>) {
        let manager = self.capture.client.bind("jwm_remote_popup_manager_v1", 1);
        let lease = self.capture.client.new_id();
        let source = self.capture.client.new_id();
        self.capture
            .client
            .request(manager, 1, &[lease, self.capture.source]);
        self.capture.client.request(lease, 1, &[source]);
        let events = fence(&mut self.server, &mut self.capture.client);
        let epoch = events
            .iter()
            .find(|e| e.sender == lease && e.opcode == 0)
            .expect("prepared grant")
            .args[0];
        let other = App::with_size_color(&mut self.server, [0, 255, 255, 255], W, H);
        (
            Fixture {
                server: self.server,
                app: self.app,
                other,
                capture: self.capture,
                source,
                lease,
                epoch,
                seat: self.seat_id,
                seat_global: self.global,
                pointer: self.pointer,
            },
            self.seat,
        )
    }
}
fn no_pointer(events: &[Event], pointer: u32) {
    assert!(
        !events
            .iter()
            .any(|e| e.sender == pointer && matches!(e.opcode, 0 | 2..=4 | 6..=9)),
        "retired/dormant receiver received input"
    );
}
#[test]
fn prepared_original_ten_second_timeout_retires_without_input_or_reprepare() {
    let mut p = Prepared::new();
    let created = p
        .server
        .state
        .remote_popup
        .prepared
        .as_ref()
        .unwrap()
        .created;
    let deadline = created + Duration::from_secs(10);
    let mut removed = false;
    while p.seat.get_pointer().is_some() {
        p.server.dispatch_timers(Duration::from_millis(10));
        let events = fence(&mut p.server, &mut p.app.client);
        no_pointer(&events, p.pointer);
        removed |= events
            .iter()
            .any(|e| e.sender == 2 && e.opcode == 1 && e.args == [p.global]);
        if p.seat.get_pointer().is_none() {
            assert!(Instant::now() >= deadline);
        }
        assert!(Instant::now() < deadline + Duration::from_secs(2));
    }
    assert!(removed);
    assert!(p.server.state.remote_popup.prepared.is_none());
    let owner = p
        .server
        .state
        .surface_for_window(p.app.window)
        .unwrap()
        .client()
        .unwrap()
        .id();
    assert!(super::super::prepare_dormant(&mut p.server.state, owner).is_none());
}
#[test]
fn prepared_owner_disconnect_retires_before_preparation_timeout() {
    let p = Prepared::new();
    let Prepared {
        mut server,
        app,
        seat,
        ..
    } = p;
    let created = server.state.remote_popup.prepared.as_ref().unwrap().created;
    drop(app);
    while seat.get_pointer().is_some() {
        server.dispatch_timers(Duration::from_millis(10));
        assert!(
            Instant::now() < created + Duration::from_secs(2),
            "disconnect cleanup, not ten-second expiry"
        );
    }
    assert!(server.state.remote_popup.prepared.is_none());
    assert!(!super::super::active(&server.state));
}
#[test]
fn consumed_prepared_seat_retires_old_receiver_and_rejects_late_frame_action() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let (mut f, seat) = Prepared::new().grant();
    let (frame, _) = f.frame(&mut gl);
    f.arm(frame);
    let (old, _) = f.frame(&mut gl);
    super::super::revoke(&mut f.server.state, super::super::DENIED);
    let events = fence(&mut f.server, &mut f.capture.client);
    assert!(events.iter().any(|e| e.sender == f.lease && e.opcode == 3));
    let events = fence(&mut f.server, &mut f.app.client);
    no_pointer(&events, f.pointer); // leave is permitted; no motion/button/axis.
    assert!(
        events
            .iter()
            .any(|e| e.sender == 2 && e.opcode == 1 && e.args == [f.seat_global])
    );
    assert!(seat.get_pointer().is_none());
    let events = f.action(old, 1, 1, 5, 5);
    assert!(!events.iter().any(|e| e.sender == f.lease && e.opcode == 2));
    no_pointer(&fence(&mut f.server, &mut f.app.client), f.pointer);
}
#[test]
fn consumed_owner_new_root_cannot_reauthorize_retired_preparation() {
    let (mut f, seat) = Prepared::new().grant();
    f.capture.client.request(f.lease, 0, &[]);
    fence(&mut f.server, &mut f.capture.client);
    assert!(seat.get_pointer().is_none());
    // Remove both old roots so fresh capture necessarily selects the new root.
    f.app.client.request(f.app.toplevel, 0, &[]);
    f.app.client.request(f.app.xdg, 0, &[]);
    f.app.client.request(f.app.surface, 0, &[]);
    f.other.client.request(f.other.toplevel, 0, &[]);
    fence(&mut f.server, &mut f.app.client);
    fence(&mut f.server, &mut f.other.client);
    let compositor = f.app.client.bind("wl_compositor", 6);
    let shell = f.app.client.bind("xdg_wm_base", 6);
    let surface = f.app.client.new_id();
    f.app.client.request(compositor, 0, &[surface]);
    let xdg = f.app.client.new_id();
    f.app.client.request(shell, 2, &[xdg, surface]);
    let top = f.app.client.new_id();
    f.app.client.request(xdg, 1, &[top]);
    f.app.client.request(surface, 6, &[]);
    fence(&mut f.server, &mut f.app.client);
    assert_eq!(f.server.state.toplevels.len(), 1);
    let window = *f.server.state.toplevels.keys().next().unwrap();
    let geometry = f.server.state.window_geometry.get_mut(&window).unwrap();
    geometry.w = W;
    geometry.h = H;
    f.server.state.toplevels[&window]
        .with_pending_state(|s| s.size = Some((W as i32, H as i32).into()));
    f.server.state.toplevels[&window].send_configure();
    let events = fence(&mut f.server, &mut f.app.client);
    let serial = events
        .iter()
        .filter(|e| e.sender == xdg && e.opcode == 0)
        .next_back()
        .unwrap()
        .args[0];
    f.app.client.request(xdg, 4, &[serial]);
    let (buffer, _file) = buffer(&mut f.server, &mut f.app.client, (W, H), [0, 0, 255, 255]);
    f.app.client.request(surface, 1, &[buffer, 0, 0]);
    f.app.client.request(surface, 6, &[]);
    fence(&mut f.server, &mut f.app.client);
    let mut capture = Capture::new_with_size(&mut f.server, (W, H));
    let manager = capture.client.bind("jwm_remote_popup_manager_v1", 1);
    let lease = capture.client.new_id();
    capture.client.request(manager, 1, &[lease, capture.source]);
    let events = fence(&mut f.server, &mut capture.client);
    assert!(
        events
            .iter()
            .any(|e| e.sender == lease && e.opcode == 3 && e.args == [super::super::DENIED])
    );
    assert!(!events.iter().any(|e| e.sender == lease && e.opcode == 0));
    assert!(!super::super::active(&f.server.state));
    no_pointer(&fence(&mut f.server, &mut f.app.client), f.pointer);
}
