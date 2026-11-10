//! Owned normal input-method-v2 + exact text-input-v3 recipients, actual capture GL.
//! No global enabled/focus fallback and no compositor-global keyboard injection.
use crate::backend::wayland_udev::{
    backend::fulfill_test_toplevel_capture,
    compositor::headless_render::HeadlessSmithayGles,
    image_copy_capture::{
        authority_tests::{App, Capture, shm_buffer_file},
        init_image_copy_capture,
        wire_test_client::{Client, Event, Server},
    },
};
use smithay::utils::SERIAL_COUNTER;

pub(crate) fn roundtrip(server: &mut Server) {
    server.roundtrip();
    server.roundtrip();
}
pub(crate) fn events(client: &mut Client) -> Vec<Event> {
    let events = client.events();
    assert!(
        events.iter().all(|e| !(e.sender == 1 && e.opcode == 0)),
        "unexpected wl_display error"
    );
    events
}
fn bytes_words(bytes: &[u8]) -> Vec<u32> {
    let mut data = bytes.to_vec();
    let len = data.len() as u32;
    data.resize(data.len().next_multiple_of(4), 0);
    let mut words = vec![len];
    words.extend(
        data.chunks_exact(4)
            .map(|c| u32::from_ne_bytes(c.try_into().unwrap())),
    );
    words
}
fn string_words(text: &str) -> Vec<u32> {
    let mut bytes = text.as_bytes().to_vec();
    bytes.push(0);
    bytes_words(&bytes)
}
pub(crate) fn wire_string(args: &[u32]) -> String {
    let len = args[0] as usize;
    assert!(len > 0);
    let bytes: Vec<_> = args[1..]
        .iter()
        .flat_map(|word| word.to_ne_bytes())
        .collect();
    assert_eq!(bytes[len - 1], 0);
    String::from_utf8(bytes[..len - 1].to_vec()).unwrap()
}
pub(crate) fn focus(server: &mut Server, app: &App) {
    let surface = server.state.surface_for_window(app.window).unwrap();
    server
        .state
        .set_keyboard_focus(Some(surface), SERIAL_COUNTER.next_serial());
    roundtrip(server);
}
/// Actual normal IME resource, never a test-set enabled boolean. This helper
/// is shared by native authority tests and authenticated TCP/viewer tests.
pub(crate) struct TextContext {
    pub(crate) text: u32,
    pub(crate) ime: Option<Client>,
    pub(crate) im: u32,
    pub(crate) serial: u32,
}
impl TextContext {
    pub(crate) fn new(server: &mut Server, app: &mut App, with_ime: bool) -> Self {
        let mut ime = with_ime.then(|| server.connect());
        let mut im = 0;
        let mut serial = 0;
        if let Some(client) = ime.as_mut() {
            let seat = client.bind("wl_seat", 7);
            let manager = client.bind("zwp_input_method_manager_v2", 1);
            im = client.new_id();
            client.request(manager, 0, &[seat, im]);
            roundtrip(server);
            let startup = events(client);
            assert!(
                !startup.iter().any(|e| e.sender == im && e.opcode == 6),
                "owned normal IME must be available"
            );
            serial += startup
                .iter()
                .filter(|e| e.sender == im && e.opcode == 5)
                .count() as u32;
        }
        let seat = app.client.bind("wl_seat", 7);
        let keyboard = app.client.new_id();
        app.client.request(seat, 1, &[keyboard]);
        let manager = app.client.bind("zwp_text_input_manager_v3", 1);
        let text = app.client.new_id();
        app.client.request(manager, 1, &[text, seat]);
        roundtrip(server);
        focus(server, app);
        app.client.request(text, 1, &[]);
        app.client.request(text, 7, &[]);
        roundtrip(server);
        if let Some(client) = ime.as_mut() {
            let activation = events(client);
            assert!(
                activation.iter().any(|e| e.sender == im && e.opcode == 0),
                "real IME activate required"
            );
            serial += activation
                .iter()
                .filter(|e| e.sender == im && e.opcode == 5)
                .count() as u32;
            assert!(serial > 0, "real IME done required");
        }
        events(&mut app.client);
        Self {
            text,
            ime,
            im,
            serial,
        }
    }
    pub(crate) fn take_commits(&self, app: &mut App) -> Vec<String> {
        let relevant: Vec<_> = events(&mut app.client)
            .into_iter()
            .filter(|e| e.sender == self.text)
            .collect();
        let mut commits = Vec::new();
        let mut pending = false;
        for e in relevant {
            match e.opcode {
                3 => {
                    assert!(!pending, "commit must end with done before another commit");
                    commits.push(wire_string(&e.args));
                    pending = true;
                }
                5 => {
                    assert!(
                        pending,
                        "remote commit done must follow exactly one commit_string"
                    );
                    pending = false;
                }
                0 | 1 => {}
                other => panic!(
                    "unexpected text operation {other}; remote text must not inject preedit/deletion"
                ),
            }
        }
        assert!(
            !pending,
            "native commit_string must be followed by done in the same dispatch batch"
        );
        commits
    }
    fn pending_ime_text(&mut self, server: &mut Server) {
        self.ime
            .as_mut()
            .unwrap()
            .request(self.im, 0, &string_words("pending IME"));
        roundtrip(server);
    }
    fn preedit(&mut self, server: &mut Server) {
        let mut args = string_words("输入中");
        args.extend([0, 9]);
        let client = self.ime.as_mut().unwrap();
        client.request(self.im, 1, &args);
        client.request(self.im, 3, &[self.serial]);
        roundtrip(server);
    }
    fn aba(&mut self, server: &mut Server, app: &mut App) {
        app.client.request(self.text, 2, &[]);
        app.client.request(self.text, 7, &[]);
        roundtrip(server);
        app.client.request(self.text, 1, &[]);
        app.client.request(self.text, 7, &[]);
        roundtrip(server);
        events(&mut app.client);
    }
}
struct Fixture {
    server: Server,
    app: App,
    other: App,
    other_text: u32,
    context: TextContext,
    capture: Capture,
    frame: u32,
    lease: u32,
    epoch: Option<u32>,
}
impl Fixture {
    fn new(gl: &mut HeadlessSmithayGles, with_ime: bool) -> Self {
        let mut server = Server::new();
        server.state.image_capture_pending =
            Some(init_image_copy_capture(&server.display.handle()));
        let mut app = App::new(&mut server);
        let context = TextContext::new(&mut server, &mut app, with_ime);
        let mut capture = Capture::new(&mut server);
        let mut other = App::new(&mut server);
        let other_seat = other.client.bind("wl_seat", 7);
        let other_manager = other.client.bind("zwp_text_input_manager_v3", 1);
        let other_text = other.client.new_id();
        other
            .client
            .request(other_manager, 1, &[other_text, other_seat]);
        roundtrip(&mut server);
        events(&mut other.client);
        let (buffer, _file) = shm_buffer_file(&mut server, &mut capture.client, [0x5a; 4]);
        let frame = capture.client.new_id();
        capture.client.request(capture.session, 0, &[frame]);
        capture.client.request(frame, 1, &[buffer]);
        capture.client.request(frame, 3, &[]);
        roundtrip(&mut server);
        let queue = server.state.image_capture_pending.as_ref().unwrap().clone();
        fulfill_test_toplevel_capture(&mut gl.renderer, &mut None, &server.state, &queue);
        roundtrip(&mut server);
        assert!(
            events(&mut capture.client)
                .iter()
                .any(|e| e.sender == frame && e.opcode == 3)
        );
        let manager = capture.client.bind("jwm_remote_text_manager_v1", 1);
        let lease = capture.client.new_id();
        capture.client.request(manager, 1, &[lease, capture.source]);
        roundtrip(&mut server);
        let reply = events(&mut capture.client);
        let epoch = reply
            .iter()
            .find(|e| e.sender == lease && e.opcode == 0)
            .map(|e| {
                assert_eq!(e.args.len(), 3);
                assert!(e.args[2] > 0 && e.args[2] <= 30_000);
                e.args[1]
            });
        if with_ime {
            assert!(
                epoch.is_some(),
                "clean active TIv3+IME must grant; reply opcodes: {:?}",
                reply
                    .iter()
                    .map(|e| (e.sender, e.opcode))
                    .collect::<Vec<_>>()
            );
        } else {
            assert!(epoch.is_none());
            assert!(reply.iter().any(|e| e.sender == lease && e.opcode == 2));
        }
        Self {
            server,
            app,
            other,
            other_text,
            context,
            capture,
            frame,
            lease,
            epoch,
        }
    }
    fn commit(&mut self, text: &[u8], wrong_epoch: bool, frame: u32) -> Vec<Event> {
        let mut args = vec![self.epoch.unwrap_or(0) + u32::from(wrong_epoch), frame];
        args.extend(bytes_words(text));
        self.capture.client.request(self.lease, 1, &args);
        roundtrip(&mut self.server);
        events(&mut self.capture.client)
    }
    fn assert_bystander_empty(&mut self) {
        assert!(
            !events(&mut self.other.client)
                .iter()
                .any(|e| e.sender == self.other_text && matches!(e.opcode, 2..=5)),
            "bystander TI must receive no preedit/commit/delete/done"
        );
    }
    fn assert_none(&mut self) {
        assert!(self.context.take_commits(&mut self.app).is_empty());
        self.assert_bystander_empty();
    }
}
#[test]
fn native_text_exact_utf8_commit_done_once_then_replay_denied() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    for text in ["中文输入", "e\u{301}", "👩🏽‍💻🙂", "中文 e\u{301} 👩🏽‍💻"]
    {
        let mut f = Fixture::new(&mut gl, true);
        let reply = f.commit(text.as_bytes(), false, f.frame);
        assert!(reply.iter().any(|e| e.sender == f.lease && e.opcode == 1));
        assert_eq!(f.context.take_commits(&mut f.app), [text]);
        f.assert_bystander_empty();
        let reply = f.commit(text.as_bytes(), false, f.frame);
        assert!(!reply.iter().any(|e| e.sender == f.lease && e.opcode == 1));
        f.assert_none();
    }
}
#[test]
fn native_text_denies_absent_ime_invalid_payload_and_wrong_epoch() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut no_ime = Fixture::new(&mut gl, false);
    no_ime.assert_none();
    for bytes in [vec![], vec![0], vec![0xff], vec![b'a'; 4001]] {
        let mut f = Fixture::new(&mut gl, true);
        let reply = f.commit(&bytes, false, f.frame);
        assert!(!reply.iter().any(|e| e.sender == f.lease && e.opcode == 1));
        f.assert_none();
    }
    let mut f = Fixture::new(&mut gl, true);
    let reply = f.commit("中文".as_bytes(), true, f.frame);
    assert!(!reply.iter().any(|e| e.sender == f.lease && e.opcode == 1));
    f.assert_none();
}
#[test]
fn native_text_context_aba_half_ime_batch_preedit_owner_lock_unmap_fail_closed() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    for case in 0..6 {
        let mut f = Fixture::new(&mut gl, true);
        match case {
            0 => f.context.aba(&mut f.server, &mut f.app),
            1 => {
                f.context.pending_ime_text(&mut f.server);
                // The normal IME forwards its commit_string immediately, but
                // has deliberately not ended the batch. Establish this local
                // prefix before testing that remote text cannot append to it
                // or finish it with done.
                let prefix: Vec<_> = events(&mut f.app.client)
                    .into_iter()
                    .filter(|e| e.sender == f.context.text)
                    .collect();
                assert_eq!(prefix.len(), 1, "only the known local IME prefix");
                assert_eq!(prefix[0].opcode, 3);
                assert_eq!(wire_string(&prefix[0].args), "pending IME");
            }
            2 => {
                f.context.preedit(&mut f.server);
                let preedit = events(&mut f.app.client);
                assert!(
                    preedit.iter().any(|e| e.sender == f.context.text
                        && e.opcode == 2
                        && wire_string(&e.args) == "输入中"),
                    "normal IME preedit must actually reach TI before denial check"
                );
                assert!(
                    preedit
                        .iter()
                        .any(|e| e.sender == f.context.text && e.opcode == 5)
                );
            }
            3 => focus(&mut f.server, &f.other),
            4 => {
                f.server.state.session_lock_supported = true;
                let mut locker = f.server.connect();
                let manager = locker.bind("ext_session_lock_manager_v1", 1);
                let lock = locker.new_id();
                locker.request(manager, 1, &[lock]);
                roundtrip(&mut f.server);
                assert!(f.server.state.session_locked);
            }
            5 => {
                f.app.unmap(&mut f.server);
                f.app.remap(&mut f.server);
                events(&mut f.app.client);
            }
            _ => unreachable!(),
        }
        let reply = f.commit("不得泄露".as_bytes(), false, f.frame);
        assert!(
            !reply.iter().any(|e| e.sender == f.lease && e.opcode == 1),
            "unsafe context {case} accepted"
        );
        f.assert_none();
    }
}

#[test]
fn native_text_expired_lease_and_unrendered_frame_never_commit() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut f = Fixture::new(&mut gl, true);
    f.server.state.remote_text.active.as_mut().unwrap().created =
        std::time::Instant::now() - std::time::Duration::from_secs(31);
    let reply = f.commit(b"expired", false, f.frame);
    assert!(!reply.iter().any(|e| e.sender == f.lease && e.opcode == 1));
    f.assert_none();
    let mut f = Fixture::new(&mut gl, true);
    // A session permits only one live frame, including completed frames.
    // Destroy the old object before creating the legal, unsubmitted proof.
    f.capture.client.request(f.frame, 0, &[]);
    roundtrip(&mut f.server);
    events(&mut f.capture.client);
    let unrendered = f.capture.client.new_id();
    f.capture
        .client
        .request(f.capture.session, 0, &[unrendered]);
    roundtrip(&mut f.server);
    events(&mut f.capture.client); // creation itself must not be a protocol error
    let reply = f.commit(b"not-ready", false, unrendered);
    assert!(!reply.iter().any(|e| e.sender == f.lease && e.opcode == 1));
    assert!(
        reply.iter().any(|e| e.sender == f.lease && e.opcode == 2),
        "legal unrendered frame must receive a lease rejection, not disconnect"
    );
    f.assert_none();
}

#[test]
fn native_text_exact_byte_limit_and_multibyte_utf8() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    for text in ["a".repeat(4000), "中".repeat(1333)] {
        let mut f = Fixture::new(&mut gl, true);
        let reply = f.commit(text.as_bytes(), false, f.frame);
        assert!(reply.iter().any(|e| e.sender == f.lease && e.opcode == 1));
        assert_eq!(f.context.take_commits(&mut f.app), [text]);
        f.assert_bystander_empty();
    }
    for bytes in ["中".repeat(1334).into_bytes(), b"before\0after".to_vec()] {
        let mut f = Fixture::new(&mut gl, true);
        let reply = f.commit(&bytes, false, f.frame);
        assert!(!reply.iter().any(|e| e.sender == f.lease && e.opcode == 1));
        f.assert_none();
    }
}
