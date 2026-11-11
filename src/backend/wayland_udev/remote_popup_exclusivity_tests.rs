//! Real private protocol admission with separate fresh capture connections.
use super::*;
use crate::backend::wayland_udev::{
    remote_control, remote_keyboard, remote_pointer, remote_popup, remote_text,
};
use smithay::{backend::input::InputTime, input::pointer::MotionEvent, utils::SERIAL_COUNTER};
#[derive(Clone, Copy, Debug)]
enum Kind {
    Keyboard,
    Pointer,
    Control,
    Text,
}
impl Kind {
    fn manager(self) -> &'static str {
        match self {
            Self::Keyboard => "jwm_remote_keyboard_manager_v1",
            Self::Pointer => "jwm_remote_pointer_manager_v1",
            Self::Control => "jwm_remote_control_manager_v1",
            Self::Text => "jwm_remote_text_manager_v1",
        }
    }
    fn active(self, state: &crate::backend::wayland::state::JwmWaylandState) -> bool {
        match self {
            Self::Keyboard => remote_keyboard::active(state),
            Self::Pointer => remote_pointer::active(state),
            Self::Control => remote_control::active(state),
            Self::Text => remote_text::active(state),
        }
    }
    fn revoked(self) -> u16 {
        match self {
            Self::Keyboard | Self::Control => 4,
            Self::Pointer | Self::Text => 2,
        }
    }
}
fn setup(
    kind: Kind,
) -> (
    Server,
    App,
    Capture,
    Capture,
    Option<remote_text::tests::TextContext>,
) {
    let mut server = Server::new();
    server.state.image_capture_pending = Some(init_image_copy_capture(&server.display.handle()));
    let mut app = App::with_size_color(&mut server, [0, 0, 255, 255], W, H);
    let first = Capture::new_with_size(&mut server, (W, H));
    let second = Capture::new_with_size(&mut server, (W, H));
    let seat = app.client.bind("wl_seat", 7);
    let pointer = app.client.new_id();
    let keyboard = app.client.new_id();
    app.client.request(seat, 0, &[pointer]);
    app.client.request(seat, 1, &[keyboard]);
    fence(&mut server, &mut app.client);
    let surface = server.state.surface_for_window(app.window).unwrap();
    server
        .state
        .set_keyboard_focus(Some(surface.clone()), SERIAL_COUNTER.next_serial());
    let p = server.state.seat.get_pointer().unwrap();
    p.motion(
        &mut server.state,
        Some((surface, (0.0, 0.0).into())),
        &MotionEvent {
            location: (2.0, 2.0).into(),
            serial: SERIAL_COUNTER.next_serial(),
            time: InputTime::now(),
        },
    );
    p.frame(&mut server.state);
    fence(&mut server, &mut app.client);
    let text = matches!(kind, Kind::Text)
        .then(|| remote_text::tests::TextContext::new(&mut server, &mut app, true));
    (server, app, first, second, text)
}
fn grant_old(server: &mut Server, capture: &mut Capture, kind: Kind) -> (u32, Vec<Event>) {
    let manager = capture.client.bind(kind.manager(), 1);
    let lease = capture.client.new_id();
    capture.client.request(manager, 1, &[lease, capture.source]);
    (lease, fence(server, &mut capture.client))
}
fn grant_family(server: &mut Server, capture: &mut Capture) -> (u32, Vec<Event>) {
    let manager = capture.client.bind("jwm_remote_popup_manager_v1", 1);
    let lease = capture.client.new_id();
    capture.client.request(manager, 1, &[lease, capture.source]);
    (lease, fence(server, &mut capture.client))
}
#[test]
fn fresh_family_source_cannot_coexist_with_any_existing_input_lease() {
    for kind in [Kind::Keyboard, Kind::Pointer, Kind::Control, Kind::Text] {
        let (mut server, _app, mut first, mut second, _context) = setup(kind);
        let (_, events) = grant_old(&mut server, &mut first, kind);
        assert!(
            kind.active(&server.state),
            "baseline {kind:?} must genuinely grant: {:?}",
            events
                .iter()
                .map(|e| (e.sender, e.opcode, &e.args))
                .collect::<Vec<_>>()
        );
        let (lease, events) = grant_family(&mut server, &mut second);
        assert!(
            events
                .iter()
                .any(|e| e.sender == lease && e.opcode == 3 && e.args == [1]),
            "exact family denial for {kind:?}"
        );
        assert!(!remote_popup::active(&server.state));
        assert!(
            kind.active(&server.state),
            "rejected second authority must not change original"
        );
    }
}
#[test]
fn active_family_rejects_all_old_grants_from_fresh_sources() {
    for kind in [Kind::Keyboard, Kind::Pointer, Kind::Control, Kind::Text] {
        let (mut server, _app, mut first, mut second, _context) = setup(kind);
        let (family, events) = grant_family(&mut server, &mut first);
        assert!(events.iter().any(|e| e.sender == family && e.opcode == 0));
        assert!(remote_popup::active(&server.state));
        let (lease, events) = grant_old(&mut server, &mut second, kind);
        assert!(
            events
                .iter()
                .any(|e| e.sender == lease && e.opcode == kind.revoked() && e.args == [1]),
            "exact {kind:?} denial"
        );
        assert!(!kind.active(&server.state));
        assert!(remote_popup::active(&server.state));
    }
}

#[test]
fn denied_other_connection_cannot_export_or_revoke_current_family() {
    let (mut server, _app, mut owner, mut stranger, _context) = setup(Kind::Pointer);
    let (active, events) = grant_family(&mut server, &mut owner);
    assert!(events.iter().any(|e| e.sender == active && e.opcode == 0));
    let original = remote_popup::lease_timing_for_test(&server.state).unwrap();
    let (denied, events) = grant_family(&mut server, &mut stranger);
    assert!(events.iter().any(|e| e.sender == denied && e.opcode == 3));
    let inert = stranger.client.new_id();
    stranger.client.request(denied, 1, &[inert]);
    fence(&mut server, &mut stranger.client);
    assert_eq!(
        remote_popup::lease_timing_for_test(&server.state),
        Some(original)
    );
    assert!(
        !fence(&mut server, &mut owner.client)
            .iter()
            .any(|e| e.sender == active && e.opcode == 3)
    );
    // The owner can still export its original frozen authority once.
    let source = owner.client.new_id();
    owner.client.request(active, 1, &[source]);
    let session = owner.client.new_id();
    owner
        .client
        .request(owner.manager, 0, &[session, source, 0]);
    let events = fence(&mut server, &mut owner.client);
    assert!(
        events
            .iter()
            .any(|e| e.sender == session && e.opcode == 0 && e.args == [W, H])
    );
    assert!(!events.iter().any(|e| e.sender == active && e.opcode == 3));
    assert_eq!(
        remote_popup::lease_timing_for_test(&server.state),
        Some(original)
    );
}
