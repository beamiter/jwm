//! Installed applications on private Wayland displays using the real CLI and TCP session.
use super::cli_process_integration::{PrivateDir, Process, accept, cli, cli_binary};
use super::*;
use crate::backend::wayland_udev::{
    image_copy_capture::wire_test_client::test_output, viewer_handoff,
};
use serde_json::{Value, json};
use smithay::output::{Mode, Scale};
use smithay::{
    backend::input::{
        Axis as InputAxis, AxisSource as InputAxisSource, ButtonState as InputButtonState,
        KeyState as BackendKeyState,
    },
    input::{
        keyboard::FilterResult,
        pointer::{AxisFrame, ButtonEvent},
    },
};
use std::{
    os::unix::net::UnixListener,
    path::{Path, PathBuf},
    process::Stdio,
};

const SIZE: (u32, u32) = (640, 480);

struct ApplicationSession {
    application: Process,
    host: Option<Process>,
    viewer: Option<Process>,
    source: Server,
    viewer_display: Server,
    source_listener: UnixListener,
    viewer_listener: UnixListener,
    source_root: Option<WlSurface>,
    viewer_root: Option<WlSurface>,
    readback: Option<Readback>,
    presented: Vec<u8>,
    suppressed: std::collections::HashSet<u8>,
    directory: PrivateDir,
}

impl ApplicationSession {
    fn start(
        gl: &mut HeadlessSmithayGles,
        directory: PrivateDir,
        executable: &Path,
        scale: i32,
        scroll: bool,
        spawn: impl FnOnce(&mut std::process::Command) -> Process,
    ) -> Self {
        let binary = cli_binary();
        let root = &directory.0;
        let source_socket = root.join("source.sock");
        let viewer_socket = root.join("viewer.sock");
        let event_path = root.join("events.jsonl");
        let key = root.join("session.key");
        let source_listener = UnixListener::bind(&source_socket).unwrap();
        let viewer_listener = UnixListener::bind(&viewer_socket).unwrap();
        source_listener.set_nonblocking(true).unwrap();
        viewer_listener.set_nonblocking(true).unwrap();
        let mut source = Server::new();
        let mut viewer_display = Server::new();
        for server in [&mut source, &mut viewer_display] {
            server.state.image_capture_pending =
                Some(init_image_copy_capture(&server.display.handle()));
        }
        register_test_presentation(&mut viewer_display);
        viewer_handoff::init(&viewer_display.display.handle());
        install_keymaps(&mut source, &mut viewer_display);
        let output = test_output("application-acceptance-output");
        let mode = Mode {
            size: (1024 * scale, 768 * scale).into(),
            refresh: 60_000,
        };
        output.change_current_state(
            Some(mode),
            None,
            Some(Scale::Integer(scale)),
            Some((0, 0).into()),
        );
        output.set_preferred(mode);
        output.create_global::<crate::backend::wayland::state::JwmWaylandState>(
            &source.display.handle(),
        );
        source.state.outputs.push(output.clone());
        source
            .state
            .output_rects
            .push(smithay::utils::Rectangle::new(
                (0, 0).into(),
                (1024, 768).into(),
            ));
        let mut command = cli(executable, root);
        command
            .env("GDK_BACKEND", "wayland")
            .env("GTK_THEME", "Adwaita")
            .env("GSETTINGS_BACKEND", "memory")
            .env("XDG_CACHE_HOME", root)
            .env("XDG_STATE_HOME", root)
            .env("XDG_DATA_HOME", root)
            .env("WAYLAND_DISPLAY", &source_socket);
        let application = spawn(&mut command);
        let mut session = ApplicationSession {
            application,
            host: None,
            viewer: None,
            source,
            viewer_display,
            source_listener,
            viewer_listener,
            source_root: None,
            viewer_root: None,
            readback: None,
            presented: Vec::new(),
            suppressed: Default::default(),
            directory,
        };
        session.wait(gl, "application initial toplevel", |s| {
            assert!(
                !s.application.exited(),
                "application exited: {}",
                s.application.text()
            );
            !s.source.state.toplevels.is_empty()
        });
        let surface = configure(&mut session.source);
        session.source_root = Some(surface.clone());
        output.enter(&surface);
        session.wait(gl, "mapped application with both input devices", |s| {
            with_renderer_surface_state(&surface, |state| state.buffer().is_some()).unwrap_or(false)
                && s.source
                    .state
                    .seat
                    .get_keyboard()
                    .unwrap()
                    .client_keyboards(&surface.client().unwrap())
                    .next()
                    .is_some()
                && focus_pointer(&mut s.source, &surface)
        });
        assert_eq!(
            with_renderer_surface_state(&surface, |state| state.buffer_scale()),
            Some(scale),
            "application must commit a buffer at the requested output scale"
        );
        session
            .source
            .state
            .set_keyboard_focus(Some(surface), SERIAL_COUNTER.next_serial());
        let root = &session.directory.0;
        assert!(
            cli(&binary, root)
                .args(["keygen", "--output"])
                .arg(&key)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success()
        );
        let mut host_command = cli(&binary, root);
        host_command
            .args([
                "host",
                "--wayland-select",
                "--once",
                "--listen",
                "127.0.0.1:0",
                "--wayland-display",
            ])
            .arg(&source_socket)
            .arg("--key-file")
            .arg(&key)
            .args(["--allow-wayland-control", "--allow-wayland-gestures"]);
        if scroll {
            host_command.arg("--allow-wayland-scroll");
        }
        let mut host = Process::spawn(&mut host_command);
        host.child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(b"1\n")
            .unwrap();
        session.host = Some(host);
        session.wait(gl, "host publishes its authenticated TCP listener", |s| {
            s.host.as_ref().unwrap().text().contains("listening on ")
        });
        let log = session.host.as_ref().unwrap().text();
        let address: std::net::SocketAddr = log
            .split("listening on ")
            .nth(1)
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert!(address.ip().is_loopback());
        let mut viewer_command = cli(&binary, &session.directory.0);
        viewer_command
            .args([
                "session",
                &address.to_string(),
                "--viewer-backend",
                "wayland",
                "--window-size",
                "640x480",
                "--viewer-wayland-display",
            ])
            .arg(viewer_socket)
            .arg("--key-file")
            .arg(&key)
            .args(["--control", "--wayland-gestures"]);
        if scroll {
            viewer_command.arg("--wayland-scroll");
        }
        session.viewer = Some(Process::spawn_with_stdout(
            &mut viewer_command,
            File::create(event_path).unwrap().into(),
        ));
        session.wait(gl, "authenticated presentation and gesture lease", |s| {
            s.snapshot()["wayland_gestures"]["ready_frame"].is_u64() && !s.presented.is_empty()
        });
        session
    }

    fn disconnect(&mut self, gl: &mut HeadlessSmithayGles) {
        let stdin = self.viewer.as_mut().unwrap().child.stdin.as_mut().unwrap();
        writeln!(stdin, "{}", json!({"command": "disconnect", "request": 1})).unwrap();
        self.wait(gl, "graceful managed viewer and host exit", |s| {
            let viewer = s.viewer.as_mut().unwrap().exited();
            s.host.as_mut().unwrap().exited() && viewer
        });
        for process in [self.host.as_ref().unwrap(), self.viewer.as_ref().unwrap()] {
            assert!(process.status.unwrap().success(), "{}", process.text());
        }
    }

    fn wait(
        &mut self,
        gl: &mut HeadlessSmithayGles,
        phase: &str,
        mut ready: impl FnMut(&mut Self) -> bool,
    ) {
        let deadline = Instant::now() + Duration::from_secs(12);
        while !ready(self) {
            self.tick(gl);
            assert!(
                Instant::now() < deadline,
                "{phase} timed out; application={} host={} viewer={} snapshot={}",
                self.application.text(),
                self.host.as_ref().map(Process::text).unwrap_or_default(),
                self.viewer.as_ref().map(Process::text).unwrap_or_default(),
                self.snapshot()
            );
            thread::sleep(Duration::from_millis(4));
        }
    }

    fn tick(&mut self, gl: &mut HeadlessSmithayGles) {
        accept(&self.source_listener, &mut self.source);
        accept(&self.viewer_listener, &mut self.viewer_display);
        self.source.dispatch_timers(Duration::ZERO);
        self.source.roundtrip();
        if let Some(surface) = self.source_root.as_ref() {
            let output = &self.source.state.outputs[0];
            smithay::desktop::utils::send_frames_surface_tree(
                surface,
                output,
                Duration::from_millis(u64::from(InputTime::now().millis())),
                None,
                |_, _| Some(output.clone()),
            );
        }
        fulfill_test_toplevel_capture(
            &mut gl.renderer,
            &mut None,
            &self.source.state,
            self.source.state.image_capture_pending.as_ref().unwrap(),
        );
        self.source.roundtrip();
        let local = &mut self.viewer_display;
        local.dispatch_timers(Duration::ZERO);
        local.roundtrip();
        if self.viewer_root.is_none() && !local.state.toplevels.is_empty() {
            let surface = configure(local);
            local
                .state
                .set_keyboard_focus(Some(surface.clone()), SERIAL_COUNTER.next_serial());
            self.viewer_root = Some(surface);
        }
        if let Some(surface) = self
            .viewer_root
            .as_ref()
            .filter(|surface| surface.is_alive())
        {
            let feedback = if focus_pointer(local, surface) {
                take_feedback(surface)
            } else {
                Vec::new()
            };
            if !feedback.is_empty() {
                let readback = self
                    .readback
                    .get_or_insert_with(|| Readback::new(local, SIZE));
                if let Some(pixels) = readback.sample(local, gl) {
                    self.presented = pixels;
                    present(local, feedback);
                } else {
                    for callback in feedback {
                        callback.discarded();
                    }
                }
            }
        }
    }

    fn key(&mut self, code: u32, down: bool) {
        let local = &mut self.viewer_display;
        let state = if down {
            BackendKeyState::Pressed
        } else {
            BackendKeyState::Released
        };
        let serial = SERIAL_COUNTER.next_serial();
        let time = InputTime::now();
        let key = (code + 8).into();
        if crate::backend::wayland_udev::backend::route_viewer_handoff_keyboard(
            &mut local.state,
            key,
            state,
            serial,
            time,
            &crate::config::CONFIG.load(),
            &mut self.suppressed,
        )
        .is_none()
        {
            local
                .state
                .keyboard_input_for_popup(key, state, serial, time, |_, _, _| {
                    FilterResult::Forward
                });
        }
        local.roundtrip();
    }

    fn chord(&mut self, gl: &mut HeadlessSmithayGles, modifiers: &[u32], key: u32) {
        for &modifier in modifiers {
            self.key(modifier, true);
        }
        self.key(key, true);
        self.key(key, false);
        for &modifier in modifiers.iter().rev() {
            self.key(modifier, false);
        }
        self.pump_input(gl);
    }

    fn pump_input(&mut self, gl: &mut HeadlessSmithayGles) {
        // Keep dispatching both displays between input actions to let the
        // host drain its bounded control queue while the application repaints.
        let until = Instant::now() + Duration::from_millis(85);
        while Instant::now() < until {
            self.tick(gl);
            thread::sleep(Duration::from_millis(4));
        }
    }

    fn move_pointer(&mut self, location: (f64, f64)) {
        let surface = self.viewer_root.as_ref().expect("mapped viewer root");
        assert!(point_in_root(&mut self.viewer_display, surface, location));
        self.viewer_display.roundtrip();
    }

    fn left_button(&mut self, down: bool) {
        let viewer = &mut self.viewer_display;
        let pointer = viewer.state.seat.get_pointer().unwrap();
        pointer.button(
            &mut viewer.state,
            &ButtonEvent {
                button: 0x110,
                state: if down {
                    InputButtonState::Pressed
                } else {
                    InputButtonState::Released
                },
                serial: SERIAL_COUNTER.next_serial(),
                time: InputTime::now(),
            },
        );
        pointer.frame(&mut viewer.state);
        viewer.roundtrip();
    }

    fn wheel(&mut self, step: i32) {
        assert!(matches!(step, -1 | 1));
        let viewer = &mut self.viewer_display;
        let pointer = viewer.state.seat.get_pointer().unwrap();
        pointer.axis(
            &mut viewer.state,
            AxisFrame::new(InputTime::now())
                .source(InputAxisSource::Wheel)
                .v120(InputAxis::Vertical, step * 120)
                .value(InputAxis::Vertical, f64::from(step) * 10.0),
        );
        pointer.frame(&mut viewer.state);
        viewer.roundtrip();
    }

    fn pointer_proof(&mut self, gl: &mut HeadlessSmithayGles, after: Option<u64>) -> u64 {
        let mut ready = None;
        self.wait(
            gl,
            "authenticated presentation proof for the next pointer action",
            |app| {
                ready = app.snapshot()["wayland_gestures"]["ready_frame"]
                    .as_u64()
                    .filter(|sequence| after.is_none_or(|previous| *sequence > previous));
                ready.is_some()
            },
        );
        ready.unwrap()
    }

    fn snapshot(&self) -> Value {
        std::fs::read_to_string(self.directory.0.join("events.jsonl"))
            .unwrap_or_default()
            .lines()
            .rev()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find(|event| event["event"] == "snapshot")
            .unwrap_or_default()
    }
}

fn configure(server: &mut Server) -> WlSurface {
    assert_eq!(server.state.toplevels.len(), 1);
    let (&window, top) = server.state.toplevels.iter().next().unwrap();
    let geometry = server.state.window_geometry.get_mut(&window).unwrap();
    (geometry.w, geometry.h) = SIZE;
    top.with_pending_state(|state| {
        state.size = Some((SIZE.0 as i32, SIZE.1 as i32).into());
        state.states.set(TopState::Activated);
    });
    top.send_configure();
    top.wl_surface().clone()
}

fn focus_pointer(server: &mut Server, surface: &WlSurface) -> bool {
    let pointer = server.state.seat.get_pointer().unwrap();
    if pointer.current_focus().as_ref() == Some(surface) {
        return true;
    }
    point_in_root(server, surface, (80.0, 150.0))
}

fn point_in_root(server: &mut Server, surface: &WlSurface, location: (f64, f64)) -> bool {
    let pointer = server.state.seat.get_pointer().unwrap();
    let Some(owner) = surface.client() else {
        return false;
    };
    let location = location.into();
    let Some((_, hit, origin)) = server.state.surface_under(location) else {
        return false;
    };
    if &hit != surface || pointer.client_pointers(&owner).next().is_none() {
        return false;
    }
    crate::backend::wayland_udev::backend::refresh_viewer_handoff_pointer_input(&mut server.state);
    server.state.pointer_location = location;
    pointer.motion(
        &mut server.state,
        Some((hit, origin)),
        &MotionEvent {
            location,
            serial: SERIAL_COUNTER.next_serial(),
            time: InputTime::now(),
        },
    );
    pointer.frame(&mut server.state);
    true
}

fn changed_document_pixels(before: &[u8], after: &[u8]) -> usize {
    before
        .chunks_exact(4)
        .zip(after.chunks_exact(4))
        .enumerate()
        .filter(|(index, (a, b))| {
            let x = *index % SIZE.0 as usize;
            let y = *index / SIZE.0 as usize;
            (8..320).contains(&x) && (54..180).contains(&y) && a != b
        })
        .count()
}

fn changed_menu_pixels(before: &[u8], after: &[u8]) -> usize {
    before
        .chunks_exact(4)
        .zip(after.chunks_exact(4))
        .enumerate()
        .filter(|(index, (a, b))| {
            let x = *index % SIZE.0 as usize;
            let y = *index / SIZE.0 as usize;
            (278..540).contains(&x)
                && (68..450).contains(&y)
                && a[..3].iter().zip(&b[..3]).any(|(a, b)| a.abs_diff(*b) > 8)
        })
        .count()
}

#[test]
#[ignore = "requires installed Gedit, private DBus, standalone CLI and strict software GL"]
fn actual_gedit_types_chinese_and_saves_selected_document_through_managed_wayland() {
    let editor_bin = PathBuf::from(std::env::var_os("JWM_REMOTE_GEDIT_BIN").expect("required"));
    assert!(editor_bin.is_absolute() && editor_bin.is_file());
    let scale = match std::env::var("JWM_REMOTE_GEDIT_SCALE").as_deref() {
        Ok("2") => 2,
        Ok("1") | Err(std::env::VarError::NotPresent) => 1,
        _ => panic!("JWM_REMOTE_GEDIT_SCALE must be 1 or 2"),
    };
    assert!(
        std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some(),
        "run under dbus-run-session"
    );
    let mut gl = HeadlessSmithayGles::new().expect("strict software GL required");
    let directory = PrivateDir::new();
    let root = &directory.0;
    let doc = root.join("acceptance.txt");
    let seed =
        "A remote editor must replace this selection.\nThis entire second line is also selected.\n";
    std::fs::write(&doc, seed).unwrap();
    let mut session =
        ApplicationSession::start(&mut gl, directory, &editor_bin, scale, false, |command| {
            Process::spawn(
                command
                    .args(["--standalone", "--encoding=UTF-8"])
                    .arg(&doc)
                    .env("GTK_IM_MODULE", "simple")
                    .env("LC_ALL", "C.UTF-8"),
            )
        });
    eprintln!("JWM_GEDIT_SOURCE_SCALE={scale}");
    let initial = session.presented.clone();
    assert!(
        initial
            .chunks_exact(4)
            .any(|pixel| pixel[..3] != initial[..3])
    );
    eprintln!("JWM_GEDIT_STAGE=initial_presentation");
    const CTRL: u32 = 29;
    const SHIFT: u32 = 42;
    session.chord(&mut gl, &[CTRL], 30); // A
    session.wait(
        &mut gl,
        "Ctrl+A selection is rendered in the text area",
        |s| changed_document_pixels(&initial, &s.presented) > 250,
    );
    eprintln!(
        "JWM_GEDIT_SELECTION_PIXELS={}",
        changed_document_pixels(&initial, &session.presented)
    );
    let selected = session.presented.clone();
    for digits in [[5, 18, 3, 32], [7, 6, 9, 8]] {
        // U+4E2D, U+6587
        session.chord(&mut gl, &[CTRL, SHIFT], 22); // GTK Unicode entry
        for digit in digits {
            session.chord(&mut gl, &[], digit);
        }
        session.chord(&mut gl, &[], 28); // Enter
    }
    session.chord(&mut gl, &[CTRL], 31); // S
    session.wait(
        &mut gl,
        "Gedit writes UTF-8 and the edited text is rendered",
        |s| {
            std::fs::read_to_string(&doc).is_ok_and(|text| text.trim_end_matches('\n') == "中文")
                && changed_document_pixels(&initial, &s.presented) > 250
                && changed_document_pixels(&selected, &s.presented) > 250
        },
    );
    eprintln!(
        "JWM_GEDIT_EDIT_PIXELS={}",
        changed_document_pixels(&initial, &session.presented)
    );
    eprintln!(
        "JWM_GEDIT_SELECTED_TO_EDITED_PIXELS={}",
        changed_document_pixels(&selected, &session.presented)
    );
    let editor = session.presented.clone();
    session.move_pointer((569.0, 23.0)); // Adwaita hamburger in the fixed 640×480 window.
    session.pump_input(&mut gl);
    let before_menu = session.pointer_proof(&mut gl, None);
    session.left_button(true);
    session.pump_input(&mut gl);
    session.left_button(false);
    session.wait(&mut gl, "Gedit menu is present in the remote video", |s| {
        changed_menu_pixels(&editor, &s.presented) > 24_000
            && !smithay::wayland::compositor::get_children(s.source_root.as_ref().unwrap())
                .is_empty()
    });
    eprintln!(
        "JWM_GEDIT_MENU_PIXELS={}",
        changed_menu_pixels(&editor, &session.presented)
    );
    let menu_frame = session.pointer_proof(&mut gl, Some(before_menu));
    session.move_pointer((339.0, 343.0)); // Preferences in the presented popover.
    session.pump_input(&mut gl);
    session.pointer_proof(&mut gl, Some(menu_frame));
    session.left_button(true);
    session.pump_input(&mut gl);
    session.left_button(false);
    session.wait(&mut gl, "menu selection opens Gedit Preferences", |s| {
        use smithay::wayland::{compositor::with_states, shell::xdg::XdgToplevelSurfaceData};

        s.source.state.toplevels.iter().any(|(window, top)| {
            s.source.state.mapped_windows.contains(window)
                && with_states(top.wl_surface(), |states| {
                    states
                        .data_map
                        .get::<XdgToplevelSurfaceData>()
                        .is_some_and(|data| {
                            data.lock()
                                .unwrap()
                                .title
                                .as_deref()
                                .is_some_and(|title| title.contains("Preferences"))
                        })
                })
        })
    });
    eprintln!("JWM_GEDIT_MENU_ACTION=preferences");
    session.disconnect(&mut gl);
}

#[path = "host_wayland_firefox_integration.rs"]
mod firefox;

#[path = "host_wayland_blender_integration.rs"]
mod blender;
