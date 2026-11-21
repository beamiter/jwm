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

pub(super) struct PrivateDir(pub(super) PathBuf);
impl PrivateDir {
    pub(super) fn new() -> Self {
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
pub(super) struct Process {
    pub(super) child: Child,
    pub(super) status: Option<ExitStatus>,
    log: Arc<Mutex<Vec<u8>>>,
    reader: Option<thread::JoinHandle<()>>,
}
impl Process {
    pub(super) fn spawn(command: &mut ProcessCommand) -> Self {
        Self::spawn_with_stdout(command, Stdio::null())
    }
    pub(super) fn spawn_with_stdout(command: &mut ProcessCommand, stdout: Stdio) -> Self {
        let mut child = command
            .stdout(stdout)
            .stderr(Stdio::piped())
            .stdin(Stdio::piped())
            .spawn()
            .expect("spawn owned acceptance process");
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
    pub(super) fn text(&self) -> String {
        String::from_utf8_lossy(&self.log.lock().unwrap()).into_owned()
    }
    pub(super) fn exited(&mut self) -> bool {
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
pub(super) fn cli_binary() -> PathBuf {
    assert_eq!(
        std::env::var("JWM_REQUIRE_HEADLESS_GL").as_deref(),
        Ok("1"),
        "CLI acceptance must require actual GL, not silently skip it"
    );
    let binary = PathBuf::from(
        std::env::var_os("JWM_REMOTE_CLI_TEST_BIN").expect("a built CLI executable is required"),
    );
    assert!(binary.is_absolute() && binary.is_file());
    binary
}

pub(super) fn cli(binary: &Path, root: &Path) -> ProcessCommand {
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
pub(super) fn accept(listener: &UnixListener, server: &mut Server) {
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
    catalog: Option<OwnedCatalog>,
    hold_feedback: bool,
    target_pointer: u32,
    target_keyboard: u32,
    received: Recipient,
    surface: Option<WlSurface>,
    viewer_initial_size: (u32, u32),
    viewport: (u32, u32),
    readback: Option<Readback>,
    readback_scale: i32,
    lossy_aspect: Option<(usize, usize)>,
    last_dmabuf: Option<bool>,
    #[cfg(feature = "remote-h264")]
    dmabuf_presentations: u64,
    readback_samples: [[u8; 4]; 3],
    presentations: u64,
    entered: bool,
    suppressed: std::collections::HashSet<u8>,
    viewer_started: Option<Instant>,
    duration: Option<u16>,
    binary: PathBuf,
    directory: PrivateDir,
}

struct OwnedCatalog {
    second: App,
    _unshared: App,
    last_presented: Option<(WlSurface, bool)>,
}

fn set_owned_title(app: &mut App, title: &str) {
    let mut bytes = title.as_bytes().to_vec();
    bytes.push(0);
    let mut words = vec![bytes.len() as u32];
    bytes.resize(bytes.len().next_multiple_of(4), 0);
    words.extend(
        bytes
            .chunks_exact(4)
            .map(|word| u32::from_ne_bytes(word.try_into().unwrap())),
    );
    app.client.request(app.toplevel, 2, &words);
}

impl CliFixture {
    fn new(host_opt_in: bool, local_version: u32) -> Self {
        Self::with_duration(host_opt_in, local_version, None, None)
    }
    fn with_duration(
        host_opt_in: bool,
        local_version: u32,
        duration: Option<u16>,
        host_max: Option<u16>,
    ) -> Self {
        Self::start(host_opt_in, local_version, duration, host_max, None, false)
    }
    fn start(
        host_opt_in: bool,
        local_version: u32,
        duration: Option<u16>,
        host_max: Option<u16>,
        catalog_control: Option<bool>,
        gestures: bool,
    ) -> Self {
        Self::start_with_host(
            host_opt_in,
            local_version,
            duration,
            host_max,
            catalog_control,
            gestures,
            |_| {},
        )
    }

    fn start_with_host(
        host_opt_in: bool,
        local_version: u32,
        duration: Option<u16>,
        host_max: Option<u16>,
        catalog_control: Option<bool>,
        gestures: bool,
        configure_host: impl FnOnce(&mut ProcessCommand),
    ) -> Self {
        let binary = cli_binary();
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
        assert!((1..=3).contains(&local_version));
        if local_version == 3 {
            viewer_handoff::init(&local.display.handle());
        } else {
            use crate::viewer_handoff_protocol::server::jwm_remote_viewer_handoff_manager_v1::JwmRemoteViewerHandoffManagerV1;
            local.display.handle().create_global::<
                crate::backend::wayland::state::JwmWaylandState,
                JwmRemoteViewerHandoffManagerV1, _>(local_version, viewer_handoff::ManagerData);
        }
        install_keymaps(&mut native, &mut local);
        let mut target = App::with_size_color(&mut native, [0, 0, 255, 255], SW, SH);
        let catalog = catalog_control.is_some().then(|| {
            let mut second = App::with_size_color(&mut native, [255, 0, 0, 255], 240, 180);
            let mut unshared = App::with_size_color(&mut native, [0, 255, 0, 255], 160, 120);
            set_owned_title(&mut target, "jwm-first-owned");
            set_owned_title(&mut second, "jwm-second-owned");
            set_owned_title(&mut unshared, "jwm-unshared");
            native.roundtrip();
            OwnedCatalog {
                second,
                _unshared: unshared,
                last_presented: None,
            }
        });
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
                "--listen",
                "127.0.0.1:0",
                "--wayland-display",
            ])
            .arg(directory.0.join("source.sock"))
            .arg("--key-file")
            .arg(key);
        if catalog_control != Some(false) {
            command.arg("--allow-wayland-control");
        }
        if gestures {
            command.arg("--allow-wayland-gestures");
        }
        if host_opt_in {
            command.arg("--allow-resumable-control");
        }
        if let Some(max) = host_max {
            command.args(["--max-wayland-control-duration", &max.to_string()]);
        }
        configure_host(&mut command);
        let mut host = Process::spawn(&mut command);
        if catalog_control.is_none() {
            // Exactly one owned source exists in the control fixtures.
            host.child
                .stdin
                .as_mut()
                .unwrap()
                .write_all(b"1\n")
                .unwrap();
        }
        Self {
            viewer: None,
            host,
            native,
            local,
            native_listener,
            local_listener,
            target,
            catalog,
            hold_feedback: false,
            target_pointer,
            target_keyboard,
            received: Recipient::default(),
            surface: None,
            viewer_initial_size: (VW, VH),
            viewport: (VW, VH),
            readback: None,
            readback_scale: 1,
            lossy_aspect: None,
            last_dmabuf: None,
            #[cfg(feature = "remote-h264")]
            dmabuf_presentations: 0,
            readback_samples: [[0; 4]; 3],
            presentations: 0,
            entered: false,
            suppressed: Default::default(),
            viewer_started: None,
            duration,
            binary,
            directory,
        }
    }
    fn viewer_command(&mut self, gl: &mut HeadlessSmithayGles, subcommand: &str) -> ProcessCommand {
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
        let (width, height) = self.viewer_initial_size;
        let mut command = cli(&self.binary, &self.directory.0);
        command
            .args([
                subcommand,
                &address.to_string(),
                "--viewer-backend",
                "wayland",
                "--window-size",
                &format!("{width}x{height}"),
                "--viewer-wayland-display",
            ])
            .arg(self.directory.0.join("viewer.sock"))
            .arg("--key-file")
            .arg(self.directory.0.join("session.key"));
        command
    }
    fn connect(&mut self, gl: &mut HeadlessSmithayGles) {
        let mut command = self.viewer_command(gl, "connect");
        command.args(["--wayland-control", "--wayland-seamless-input"]);
        if let Some(duration) = self.duration {
            command.args(["--wayland-control-duration", &duration.to_string()]);
        }
        self.viewer_started = Some(Instant::now());
        self.viewer = Some(Process::spawn(&mut command));
    }
    fn configure_viewer(&mut self, size: (u32, u32)) {
        let (&window, top) = self.local.state.toplevels.iter().next().unwrap();
        let geometry = self.local.state.window_geometry.get_mut(&window).unwrap();
        (geometry.w, geometry.h) = size;
        top.with_pending_state(|state| state.size = Some((size.0 as i32, size.1 as i32).into()));
        top.send_configure();
        self.viewport = size;
        self.readback = None;
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
                let (width, height) = self.viewer_initial_size;
                (geo.w, geo.h) = (width, height);
                top.with_pending_state(|state| {
                    state.size = Some((width as i32, height as i32).into());
                    state.states.set(TopState::Activated);
                });
                top.send_configure();
            }
        }
        self.local.roundtrip();
        if self.surface.is_none() || self.catalog.is_some() {
            let surface = self
                .local
                .state
                .toplevels
                .values()
                .next()
                .map(|top| top.wl_surface().clone());
            if self.surface != surface {
                self.readback = None;
                self.viewport = self.viewer_initial_size;
                self.surface = surface;
                self.last_dmabuf = None;
            }
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
        if !self.hold_feedback
            && let Some(surface) = self.surface.as_ref()
        {
            let feedback = take_feedback(surface);
            if !feedback.is_empty() {
                let (scale, dmabuf) = with_renderer_surface_state(surface, |state| {
                    (
                        state.buffer_scale(),
                        state.buffer().is_some_and(|buffer| {
                            smithay::wayland::dmabuf::get_dmabuf(buffer).is_ok()
                        }),
                    )
                })
                .expect("committed viewer buffer");
                if scale != self.readback_scale {
                    self.readback_scale = scale;
                    self.readback = None;
                }
                if self.readback.is_none() {
                    self.readback = Some(Readback::new(&mut self.local, self.viewport));
                }
                let pixels = self.readback.as_mut().unwrap().sample(&mut self.local, gl);
                // Readback dispatches real client requests. Closing the role
                // during that dispatch invalidates both the grant and feedback.
                if !surface.is_alive() {
                    for callback in feedback {
                        callback.discarded();
                    }
                    self.readback = None;
                    self.surface = None;
                    return;
                }
                let pixels = pixels.expect("live viewer role still has its pixel readback grant");
                let (width, height) = (self.viewport.0 as usize, self.viewport.1 as usize);
                self.readback_samples = [
                    pixels[..4].try_into().unwrap(),
                    pixels[((height / 2) * width + width / 2) * 4..][..4]
                        .try_into()
                        .unwrap(),
                    pixels[((height - 1) * width + width / 2) * 4..][..4]
                        .try_into()
                        .unwrap(),
                ];
                let is_color = |channel: usize| {
                    if let Some((aspect_w, aspect_h)) = self.lossy_aspect {
                        let content_w = width.min(height * aspect_w / aspect_h);
                        let content_h = height.min(width * aspect_h / aspect_w);
                        let left = (width - content_w) / 2;
                        let top = (height - content_h) / 2;
                        pixels.chunks_exact(4).enumerate().all(|(index, p)| {
                            let (x, y) = (index % width, index / width);
                            if (left..left + content_w).contains(&x)
                                && (top..top + content_h).contains(&y)
                            {
                                p[channel] >= 245
                                    && p[2 - channel] <= 12
                                    && p[1] <= 12
                                    && p[3] == 255
                            } else {
                                p == [0, 0, 0, 255]
                            }
                        })
                    } else {
                        pixels.chunks_exact(4).all(|p| {
                            p[channel] >= 250 && p[2 - channel] <= 3 && p[1] <= 3 && p[3] == 255
                        })
                    }
                };
                let red = is_color(2);
                if let Some(catalog) = self.catalog.as_mut() {
                    let blue = is_color(0);
                    assert!(
                        red || blue,
                        "viewer must only present the two authorized sources; samples={:?}, host={}, viewer={}",
                        self.readback_samples,
                        self.host.text(),
                        self.viewer.as_ref().unwrap().text()
                    );
                    catalog.last_presented = Some((surface.clone(), blue));
                } else {
                    assert!(
                        red,
                        "actual committed viewer pixels must match the owned red source"
                    );
                }
                self.presentations += 1;
                self.last_dmabuf = Some(dmabuf);
                #[cfg(feature = "remote-h264")]
                if dmabuf {
                    self.dmabuf_presentations += 1;
                }
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
        self.pointer_at(
            if inside {
                (160.0, 120.0)
            } else {
                (5000.0, 5000.0)
            },
            inside,
        );
    }

    fn pointer_at(&mut self, location: (f64, f64), inside: bool) {
        crate::backend::wayland_udev::backend::refresh_viewer_handoff_pointer_input(
            &mut self.local.state,
        );
        let surface = self.surface.clone().expect("real CLI viewer toplevel");
        let location = location.into();
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

    fn left_button(&mut self, pressed: bool) {
        use smithay::{backend::input::ButtonState, input::pointer::ButtonEvent};
        let pointer = self.local.state.seat.get_pointer().unwrap();
        pointer.button(
            &mut self.local.state,
            &ButtonEvent {
                button: 0x110,
                state: if pressed {
                    ButtonState::Pressed
                } else {
                    ButtonState::Released
                },
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

#[path = "host_wayland_source_cli_integration.rs"]
mod source_switching;

#[test]
#[ignore = "requires independent CLI artifact, private Unix listeners and strict GL; run dedicated script"]
fn native_cli_managed_release_keeps_same_viewer() {
    let mut gl = HeadlessSmithayGles::new().expect("strict headless GL required");
    let mut f = CliFixture::new(false, 2);
    let mut command = f.viewer_command(&mut gl, "session");
    let stdout = f.directory.0.join("events.jsonl");
    let output = File::create(&stdout).unwrap();
    f.viewer = Some(Process::spawn_with_stdout(
        command.arg("--control"),
        output.into(),
    ));
    let has_event = |name, field, expected: serde_json::Value| {
        let text = std::fs::read_to_string(&stdout).unwrap();
        text.lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .any(|value| {
                value["protocol"] == 1 && value["event"] == name && value[field] == expected
            })
    };
    f.wait(&mut gl, "first managed presentation", |s| {
        s.presentations > 0
    });
    let surface = f.surface.clone().unwrap();
    let owner = surface.client().unwrap();
    let keyboard = f.local.state.seat.get_keyboard().unwrap();
    let pointer = f.local.state.seat.get_pointer().unwrap();
    f.wait(&mut gl, "local managed input devices", |_| {
        keyboard.client_keyboards(&owner).count() == 1
            && pointer.client_pointers(&owner).count() == 1
    });
    f.local
        .state
        .set_keyboard_focus(Some(surface.clone()), SERIAL_COUNTER.next_serial());
    f.pointer(true);
    f.wait(&mut gl, "managed JSON input is active", |_| {
        has_event("snapshot", "wayland_input", "active".into())
    });
    f.key(42, true);
    f.wait(&mut gl, "real managed Shift recipient", |s| {
        s.received.keys == [(42, 1)]
    });
    let original_lease = remote_control::cli_lease_snapshot_for_test(&f.native.state).unwrap();
    let previous_presentations = f.presentations;
    f.configure_viewer((320, 240));
    f.wait(&mut gl, "CLI presents the resized buffer", |s| {
        let size =
            smithay::backend::renderer::utils::with_renderer_surface_state(&surface, |state| {
                state.buffer_size()
            })
            .flatten();
        size.is_some_and(|size| (size.w, size.h) == (320, 240))
            && s.presentations > previous_presentations
    });
    assert_eq!(f.received.keys, [(42, 1)]);
    assert_eq!(
        remote_control::cli_lease_snapshot_for_test(&f.native.state),
        Some(original_lease)
    );
    let send = |fixture: &mut CliFixture, request: &[u8]| {
        fixture
            .viewer
            .as_mut()
            .unwrap()
            .child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(request)
            .unwrap();
    };
    send(&mut f, b"{\"command\":\"release_input\",\"request\":1}\n");
    f.wait(
        &mut gl,
        "JSON release confirmed after native cleanup",
        |s| {
            has_event("command_result", "accepted", true.into())
                && has_event("snapshot", "wayland_input", "released".into())
                && s.received.keys == [(42, 1), (42, 0)]
                && !remote_control::active(&s.native.state)
        },
    );
    assert!(!f.viewer.as_mut().unwrap().exited() && !f.host.exited());
    assert_eq!(viewer_surface(&f.local), surface);
    assert_eq!(remote_control::attempts_for_test(&f.native.state), 1);
    send(&mut f, b"{\"command\":\"disconnect\",\"request\":2}\n");
    f.wait(
        &mut gl,
        "both managed CLI processes exit",
        CliFixture::exited,
    );
    assert!(f.viewer.as_ref().unwrap().status.unwrap().success());
    assert!(f.host.status.unwrap().success());
    assert!(has_event("snapshot", "state", "closed".into()));
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

#[test]
#[ignore = "requires independent CLI artifact, private Unix listeners and strict GL; run dedicated script"]
fn native_cli_explicit_long_budget_expires_without_regrant() {
    let mut gl = HeadlessSmithayGles::new().expect("strict headless GL required");
    let mut f = CliFixture::with_duration(true, 3, Some(40), Some(45));
    f.connect(&mut gl);
    f.wait(&mut gl, "extended production input gate ready", |s| {
        s.viewer
            .as_ref()
            .unwrap()
            .text()
            .contains("control generation 1 ready")
            && remote_control::cli_lease_snapshot_for_test(&s.native.state).is_some()
    });
    let original = remote_control::cli_lease_snapshot_for_test(&f.native.state).unwrap();
    assert!(original.2.duration_since(original.1) > Duration::from_secs(31));
    assert!(original.2.duration_since(original.1) <= Duration::from_secs(40));
    // Real wall time, real processes and real input. No test clock or deadline edits.
    while Instant::now() < original.1 + Duration::from_secs(31) {
        f.tick(&mut gl);
        assert!(
            !f.exited(),
            "explicit extended session ended before 31 seconds"
        );
        thread::sleep(Duration::from_millis(1));
    }
    assert!(viewer_handoff::active(&f.local.state));
    f.key(42, true);
    f.wait(&mut gl, "actual key beyond default lifetime", |s| {
        s.received.keys == [(42, 1)]
    });
    f.pointer(false);
    f.wait(&mut gl, "extended pause releases held key", |s| {
        remote_control::cli_lease_snapshot_for_test(&s.native.state)
            .is_some_and(|v| v.4 && v.6 == 0)
            && s.received.keys == [(42, 1), (42, 0)]
            && !viewer_handoff::inhibits(&s.local.state)
    });
    f.key(42, false);
    f.pointer(true);
    f.wait(&mut gl, "extended resume ready", |s| {
        s.viewer
            .as_ref()
            .unwrap()
            .text()
            .contains("jwm-remote: control generation 2 ready; original deadline unchanged")
            && remote_control::cli_lease_snapshot_for_test(&s.native.state)
                .is_some_and(|v| !v.4 && v.5 == 2)
    });
    let resumed = remote_control::cli_lease_snapshot_for_test(&f.native.state).unwrap();
    assert_eq!(
        (resumed.0, resumed.1, resumed.2),
        (original.0, original.1, original.2)
    );
    f.key(42, true);
    f.wait(&mut gl, "held key before original expiry", |s| {
        s.received.keys.len() == 3
    });
    f.wait(
        &mut gl,
        "original deadline releases both authorities and processes",
        |s| {
            s.exited()
                && !remote_control::active(&s.native.state)
                && !viewer_handoff::active(&s.local.state)
                && !viewer_handoff::inhibits(&s.local.state)
                && s.received.keys == [(42, 1), (42, 0), (42, 1), (42, 0)]
        },
    );
    assert!(
        f.viewer_started.unwrap().elapsed() >= Duration::from_millis(39_500),
        "an unrelated early failure must not masquerade as natural expiry"
    );
    let diagnostics = format!("{} {}", f.host.text(), f.viewer.as_ref().unwrap().text());
    assert!(
        diagnostics.contains("expired"),
        "natural expiry must be diagnosed: {diagnostics}"
    );
    // The disconnected surface need not remain mapped. Further scene motion
    // cannot mint native authority, and the original attempt count stays one.
    f.pointer(false);
    for _ in 0..10 {
        f.tick(&mut gl);
    }
    assert_eq!(remote_control::attempts_for_test(&f.native.state), 1);
    assert!(!remote_control::active(&f.native.state));
}
