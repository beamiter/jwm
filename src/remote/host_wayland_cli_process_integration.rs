//! Independent, unmodified CLI executables against two owned compositors.
//! Requires real private Unix listeners. Never substitutes in-process options,
//! socketpair test hooks, fabricated Ready records or a user's live display.
use super::*;
use crate::backend::wayland_udev::viewer_handoff;
use std::{
    io::{self, Read},
    os::unix::{fs::DirBuilderExt, net::UnixListener},
    path::{Path, PathBuf},
    process::{Child, Command as ProcessCommand, ExitStatus, Stdio},
};

struct PrivateDir(PathBuf);
impl PrivateDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("jwm-cli-{:016x}", rand::random::<u64>()));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        Self(path)
    }
}
impl Drop for PrivateDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
struct Process {
    child: Child,
    status: Option<ExitStatus>,
    log: Arc<Mutex<Vec<u8>>>,
    reader: Option<thread::JoinHandle<()>>,
}
impl Process {
    fn spawn(command: &mut ProcessCommand) -> Self {
        let mut child = command
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .stdin(Stdio::piped())
            .spawn()
            .expect("spawn actual jwm-remote executable");
        let mut stderr = child.stderr.take().unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        let sink = log.clone();
        let reader = thread::spawn(move || {
            let mut chunk = [0u8; 4096];
            while let Ok(count) = stderr.read(&mut chunk) {
                if count == 0 {
                    break;
                }
                let mut bytes = sink.lock().unwrap();
                bytes.extend_from_slice(&chunk[..count]);
                if bytes.len() > 65536 {
                    let excess = bytes.len() - 65536;
                    bytes.drain(..excess);
                }
            }
        });
        Self {
            child,
            status: None,
            log,
            reader: Some(reader),
        }
    }
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.log.lock().unwrap()).into_owned()
    }
    fn exited(&mut self) -> bool {
        if self.status.is_none() {
            self.status = self.child.try_wait().unwrap();
        }
        self.status.is_some()
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}
fn cli(binary: &Path, root: &Path) -> ProcessCommand {
    let mut command = ProcessCommand::new(binary);
    command
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("WAYLAND_SOCKET")
        .env_remove("XAUTHORITY")
        .env("XDG_RUNTIME_DIR", root)
        .env("XDG_CONFIG_HOME", root);
    command
}
fn accept(listener: &UnixListener, server: &mut Server) {
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                server
                    .display
                    .handle()
                    .insert_client(stream, Arc::new(JwmClientState::default()))
                    .unwrap();
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
            Err(error) => panic!("owned Wayland listener failed: {error}"),
        }
    }
}
struct CliFixture {
    // Drop children before their sockets or private key directory disappear.
    viewer: Option<Process>,
    host: Process,
    native: Server,
    local: Server,
    native_listener: UnixListener,
    local_listener: UnixListener,
    target: App,
    target_pointer: u32,
    target_keyboard: u32,
    received: Recipient,
    surface: Option<WlSurface>,
    readback: Option<Readback>,
    presentations: u64,
    entered: bool,
    suppressed: std::collections::HashSet<u8>,
    binary: PathBuf,
    directory: PrivateDir,
}
impl CliFixture {
    fn new(host_opt_in: bool, local_version: u32) -> Self {
        assert_eq!(
            std::env::var("JWM_REQUIRE_HEADLESS_GL").as_deref(),
            Ok("1"),
            "CLI acceptance must require actual GL, not silently skip it"
        );
        let binary = PathBuf::from(
            std::env::var_os("JWM_REMOTE_CLI_TEST_BIN")
                .expect("run scripts/test-remote-wayland-cli.sh with a built CLI"),
        );
        assert!(
            binary.is_absolute() && binary.is_file(),
            "explicit executable artifact required"
        );
        let directory = PrivateDir::new();
        let native_listener = UnixListener::bind(directory.0.join("source.sock"))
            .expect("real Unix sockets required; EPERM is a blocked acceptance run, not a pass");
        let local_listener = UnixListener::bind(directory.0.join("viewer.sock")).unwrap();
        native_listener.set_nonblocking(true).unwrap();
        local_listener.set_nonblocking(true).unwrap();
        let mut native = Server::new();
        let mut local = Server::new();
        native.state.image_capture_pending =
            Some(init_image_copy_capture(&native.display.handle()));
        local.state.image_capture_pending = Some(init_image_copy_capture(&local.display.handle()));
        register_test_presentation(&mut local);
        if local_version == 2 {
            viewer_handoff::init(&local.display.handle());
        } else {
            use crate::viewer_handoff_protocol::server::jwm_remote_viewer_handoff_manager_v1::JwmRemoteViewerHandoffManagerV1;
            local.display.handle().create_global::<
                crate::backend::wayland::state::JwmWaylandState,
                JwmRemoteViewerHandoffManagerV1, _>(1, viewer_handoff::ManagerData);
        }
        install_keymaps(&mut native, &mut local);
        let mut target = App::with_size_color(&mut native, [0, 0, 255, 255], SW, SH);
        let (target_pointer, target_keyboard) = bind_inputs(&mut native, &mut target, 7);
        focus(&mut native, &target);
        target.client.events();
        let key = directory.0.join("session.key");
        assert!(
            cli(&binary, &directory.0)
                .args(["keygen", "--output"])
                .arg(&key)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success(),
            "actual CLI keygen failed (key contents never logged)"
        );
        let mut command = cli(&binary, &directory.0);
        command
            .args([
                "host",
                "--wayland-select",
                "--once",
                "--allow-wayland-control",
                "--listen",
                "127.0.0.1:0",
                "--wayland-display",
            ])
            .arg(directory.0.join("source.sock"))
            .arg("--key-file")
            .arg(key);
        if host_opt_in {
            command.arg("--allow-resumable-control");
        }
        let mut host = Process::spawn(&mut command);
        // Exactly one owned source exists. No title matching or user desktop.
        host.child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(b"1\n")
            .unwrap();
        Self {
            viewer: None,
            host,
            native,
            local,
            native_listener,
            local_listener,
            target,
            target_pointer,
            target_keyboard,
            received: Recipient::default(),
            surface: None,
            readback: None,
            presentations: 0,
            entered: false,
            suppressed: Default::default(),
            binary,
            directory,
        }
    }
    fn connect(&mut self, gl: &mut HeadlessSmithayGles) {
        self.wait(gl, "actual host listener announcement", |s| {
            s.host.text().contains("listening on ")
        });
        let text = self.host.text();
        let address = text
            .split("listening on ")
            .nth(1)
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap();
        let address: std::net::SocketAddr = address.parse().unwrap();
        assert!(address.ip().is_loopback());
        let mut command = cli(&self.binary, &self.directory.0);
        command
            .args([
                "connect",
                &address.to_string(),
                "--viewer-backend",
                "wayland",
                "--wayland-control",
                "--wayland-seamless-input",
                "--window-size",
                "400x300",
                "--viewer-wayland-display",
            ])
            .arg(self.directory.0.join("viewer.sock"))
            .arg("--key-file")
            .arg(self.directory.0.join("session.key"));
        self.viewer = Some(Process::spawn(&mut command));
    }
    fn tick(&mut self, gl: &mut HeadlessSmithayGles) {
        accept(&self.native_listener, &mut self.native);
        accept(&self.local_listener, &mut self.local);
        self.native.roundtrip();
        fulfill_test_toplevel_capture(
            &mut gl.renderer,
            &mut None,
            &self.native.state,
            self.native.state.image_capture_pending.as_ref().unwrap(),
        );
        self.native.roundtrip();
        self.local.dispatch_timers(Duration::ZERO);
        self.local.roundtrip();
        for (&window, top) in &self.local.state.toplevels {
            if !top.is_initial_configure_sent() {
                let geo = self.local.state.window_geometry.get_mut(&window).unwrap();
                geo.w = VW;
                geo.h = VH;
                top.with_pending_state(|state| {
                    state.size = Some((VW as i32, VH as i32).into());
                    state.states.set(TopState::Activated);
                });
                top.send_configure();
            }
        }
        self.local.roundtrip();
        if self.surface.is_none() {
            self.surface = self
                .local
                .state
                .toplevels
                .values()
                .next()
                .map(|top| top.wl_surface().clone());
        }
        if self.surface.as_ref().is_some_and(|surface| {
            self.local
                .state
                .surface_under((160.0, 120.0).into())
                .is_some_and(|(_, hit, _)| &hit == surface)
        }) && !self.entered
            && viewer_handoff::registered_for_test(&self.local.state)
        {
            self.entered = true;
            self.pointer(true);
        }
        if let Some(surface) = self.surface.as_ref() {
            let feedback = take_feedback(surface);
            if !feedback.is_empty() {
                if self.readback.is_none() {
                    self.readback = Some(Readback::new(&mut self.local, (VW, VH)));
                }
                let pixels = self.readback.as_mut().unwrap().pixels(&mut self.local, gl);
                assert!(
                    pixels
                        .chunks_exact(4)
                        .all(|p| p[2] >= 250 && p[1] <= 3 && p[0] <= 3 && p[3] == 255),
                    "actual committed viewer pixels must match the owned red source"
                );
                self.presentations += 1;
                present(&mut self.local, feedback);
            }
        }
        self.received
            .collect(&mut self.target, self.target_pointer, self.target_keyboard);
    }
    fn wait(
        &mut self,
        gl: &mut HeadlessSmithayGles,
        phase: &str,
        mut done: impl FnMut(&mut Self) -> bool,
    ) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done(self) {
            self.tick(gl);
            assert!(
                Instant::now() < deadline,
                "{phase} timed out; host={} viewer={}",
                self.host.text(),
                self.viewer.as_ref().map(Process::text).unwrap_or_default()
            );
            thread::sleep(Duration::from_millis(1));
        }
    }
    fn pointer(&mut self, inside: bool) {
        crate::backend::wayland_udev::backend::refresh_viewer_handoff_pointer_input(
            &mut self.local.state,
        );
        let surface = self.surface.clone().expect("real CLI viewer toplevel");
        let location = if inside {
            (160.0, 120.0)
        } else {
            (5000.0, 5000.0)
        }
        .into();
        self.local.state.pointer_location = location;
        let focus = self
            .local
            .state
            .surface_under(location)
            .map(|(_, hit, origin)| (hit, origin));
        if inside {
            assert_eq!(
                focus.as_ref().map(|(hit, _)| hit),
                Some(&surface),
                "actual mapped viewer must win compositor scene hit-testing"
            );
        }
        let pointer = self.local.state.seat.get_pointer().unwrap();
        pointer.motion(
            &mut self.local.state,
            focus,
            &MotionEvent {
                location,
                serial: SERIAL_COUNTER.next_serial(),
                time: InputTime::now(),
            },
        );
        pointer.frame(&mut self.local.state);
        self.local.roundtrip();
    }

    fn key(&mut self, code: u32, down: bool) {
        use smithay::{backend::input::KeyState, input::keyboard::FilterResult};
        let state = if down {
            KeyState::Pressed
        } else {
            KeyState::Released
        };
        let serial = SERIAL_COUNTER.next_serial();
        let time = InputTime::now();
        if crate::backend::wayland_udev::backend::route_viewer_handoff_keyboard(
            &mut self.local.state,
            (code + 8).into(),
            state,
            serial,
            time,
            &crate::config::CONFIG.load(),
            &mut self.suppressed,
        )
        .is_none()
        {
            self.local.state.keyboard_input_for_popup(
                (code + 8).into(),
                state,
                serial,
                time,
                |_, _, _| FilterResult::Forward,
            );
        }
        self.local.roundtrip();
    }
    fn exited(&mut self) -> bool {
        let viewer = self.viewer.as_mut().is_some_and(Process::exited);
        self.host.exited() && viewer
    }
}

#[test]
#[ignore = "requires independent CLI artifact, private Unix listeners and strict GL; run dedicated script"]
fn native_cli_flags_reach_real_viewer_and_preserve_lease() {
    let mut gl = HeadlessSmithayGles::new().expect("strict headless GL required");
    let mut f = CliFixture::new(true, 2);
    f.connect(&mut gl);
    f.wait(
        &mut gl,
        "CLI reports actual local control gate ready",
        |s| {
            s.viewer
                .as_ref()
                .unwrap()
                .text()
                .contains("jwm-remote: control generation 1 ready; original deadline unchanged")
                && remote_control::cli_lease_snapshot_for_test(&s.native.state).is_some()
        },
    );
    let original = remote_control::cli_lease_snapshot_for_test(&f.native.state).unwrap();
    assert!(original.3 && !original.4 && original.5 == 1);
    assert!(f.presentations > 0 && viewer_handoff::active(&f.local.state));
    f.key(42, true);
    f.wait(&mut gl, "real remote Shift recipient", |s| {
        s.received.keys == [(42, 1)]
    });
    f.pointer(false);
    f.wait(&mut gl, "actual native pause and held cleanup", |s| {
        remote_control::cli_lease_snapshot_for_test(&s.native.state)
            .is_some_and(|v| v.4 && v.6 == 0)
            && s.received.keys == [(42, 1), (42, 0)]
            && !viewer_handoff::inhibits(&s.local.state)
    });
    f.key(42, false);
    f.pointer(true);
    f.wait(&mut gl, "same native lease resumes", |s| {
        remote_control::cli_lease_snapshot_for_test(&s.native.state)
            .is_some_and(|v| !v.4 && v.5 == 2)
    });
    let resumed = remote_control::cli_lease_snapshot_for_test(&f.native.state).unwrap();
    assert_eq!(
        (resumed.0, resumed.1, resumed.2),
        (original.0, original.1, original.2)
    );
    assert_eq!(remote_control::attempts_for_test(&f.native.state), 1);
    // Observe the real production main-loop readiness report, emitted only
    // after matching resume ACK, local clean fence and a fresh input proof.
    // The log grants nothing: native lease identity and actual recipient
    // delivery remain independent assertions below.
    f.wait(&mut gl, "generation-two production input gate ready", |s| {
        s.viewer
            .as_ref()
            .unwrap()
            .text()
            .contains("jwm-remote: control generation 2 ready; original deadline unchanged")
            && remote_control::cli_lease_snapshot_for_test(&s.native.state)
                .is_some_and(|v| !v.4 && v.5 == 2)
    });
    f.key(30, true);
    f.key(30, false);
    f.wait(&mut gl, "new generation actual remote keys", |s| {
        s.received.keys.len() == 4
    });
    assert_eq!(f.received.keys, [(42, 1), (42, 0), (30, 1), (30, 0)]);
    f.local
        .state
        .toplevels
        .values()
        .next()
        .unwrap()
        .send_close();
    f.local.roundtrip();
    f.wait(
        &mut gl,
        "both actual CLI processes exit and clear authority",
        |s| {
            s.exited()
                && !remote_control::active(&s.native.state)
                && !viewer_handoff::active(&s.local.state)
        },
    );
    assert_eq!(remote_control::attempts_for_test(&f.native.state), 1);
    assert!(
        f.viewer.as_ref().unwrap().status.unwrap().success(),
        "viewer close must exit successfully: {}",
        f.viewer.as_ref().unwrap().text()
    );
    assert!(
        f.host.status.unwrap().success(),
        "host --once must exit successfully: {}",
        f.host.text()
    );
}

#[test]
#[ignore = "requires independent CLI artifact, private Unix listeners and strict GL; run dedicated script"]
fn native_cli_requires_host_opt_in() {
    let mut gl = HeadlessSmithayGles::new().expect("strict headless GL required");
    let mut f = CliFixture::new(false, 2);
    f.connect(&mut gl);
    f.wait(
        &mut gl,
        "host opt-in denial closes both processes",
        CliFixture::exited,
    );
    assert!(
        f.host
            .text()
            .contains("resumable control requires both explicit permissions")
    );
    assert!(!f.viewer.as_ref().unwrap().status.unwrap().success());
    assert_eq!(remote_control::attempts_for_test(&f.native.state), 0);
    f.received.assert_no_input();
}

#[test]
#[ignore = "requires independent CLI artifact, private Unix listeners and strict GL; run dedicated script"]
fn native_cli_rejects_old_local_v1_without_fallback() {
    let mut gl = HeadlessSmithayGles::new().expect("strict headless GL required");
    let mut f = CliFixture::new(true, 1);
    f.connect(&mut gl);
    f.wait(
        &mut gl,
        "old local protocol denial closes both processes",
        CliFixture::exited,
    );
    assert!(
        f.viewer
            .as_ref()
            .unwrap()
            .text()
            .contains("JWM local handoff protocol v2 required")
    );
    assert!(!f.viewer.as_ref().unwrap().status.unwrap().success());
    assert_eq!(remote_control::attempts_for_test(&f.native.state), 0);
    assert!(!viewer_handoff::registered_for_test(&f.local.state));
    f.received.assert_no_input();
}
