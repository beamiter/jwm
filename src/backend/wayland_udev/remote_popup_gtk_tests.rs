//! CI-only real GTK3 compatibility probe. No synthetic toolkit activation.
//! Production capture completion here is not viewer presentation or DRM proof.
#![cfg(feature = "backend-wayland-udev")]
use crate::backend::wayland::state::JwmClientState;
use crate::backend::wayland_udev::{
    backend::fulfill_test_toplevel_capture,
    compositor::headless_render::HeadlessSmithayGles,
    image_copy_capture::{
        authority_tests::Capture,
        init_image_copy_capture,
        wire_test_client::{Client, Event, Server},
    },
};
use smithay::{
    backend::input::{ButtonState, InputTime},
    input::pointer::{ButtonEvent, MotionEvent},
    reexports::wayland_server::{Resource, protocol::wl_surface::WlSurface},
    utils::SERIAL_COUNTER,
    wayland::compositor::{SurfaceAttributes, TraversalAction, with_surface_tree_downward},
};
use std::{
    fs::File,
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    os::{
        fd::AsRawFd,
        unix::{net::UnixStream, process::CommandExt},
    },
    process::{Child, Command, Stdio},
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};
const W: u32 = 320;
const H: u32 = 240;
struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn fence(server: &mut Server, client: &mut Client) -> Vec<Event> {
    let done = client.new_id();
    client.request(1, 0, &[done]);
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut result = Vec::new();
    loop {
        server.roundtrip();
        let events = client.events();
        assert!(
            !events.iter().any(|e| e.sender == 1 && e.opcode == 0),
            "protocol error"
        );
        let finished = events.iter().any(|e| e.sender == done && e.opcode == 0);
        result.extend(events);
        if finished {
            return result;
        }
        assert!(Instant::now() < deadline, "display.sync timeout");
        thread::yield_now();
    }
}
fn callbacks(surface: &WlSurface, time: u32) {
    with_surface_tree_downward(
        surface,
        (),
        |_, _, _| TraversalAction::DoChildren(()),
        |_, states, _| {
            for callback in states
                .cached_state
                .get::<SurfaceAttributes>()
                .current()
                .frame_callbacks
                .drain(..)
            {
                callback.done(time);
            }
        },
        |_, _, _| true,
    );
}
fn frame(
    server: &mut Server,
    capture: &mut Capture,
    source: u32,
    gl: &mut HeadlessSmithayGles,
) -> (u32, Vec<u8>, [u32; 3]) {
    let client = &mut capture.client;
    let shm = client.bind("wl_shm", 1);
    let fd =
        nix::sys::memfd::memfd_create(c"gtk-popup-capture", nix::sys::memfd::MFdFlags::MFD_CLOEXEC)
            .unwrap();
    let bytes = W * H * 4;
    nix::unistd::ftruncate(&fd, i64::from(bytes)).unwrap();
    let mut file = File::from(fd);
    file.write_all(&vec![0x5a; bytes as usize]).unwrap();
    let pool = client.new_id();
    client.request_with_fd_args(shm, 0, &[pool, bytes], file.as_raw_fd());
    let buffer = client.new_id();
    client.request(pool, 0, &[buffer, 0, W, H, W * 4, 0]);
    let session = client.new_id();
    client.request(capture.manager, 0, &[session, source, 0]);
    let constraints = fence(server, client);
    assert!(
        constraints
            .iter()
            .any(|e| e.sender == session && e.opcode == 0 && e.args == [W, H]),
        "GTK source must match bounded root geometry"
    );
    let frame = client.new_id();
    client.request(session, 0, &[frame]);
    client.request(frame, 1, &[buffer]);
    client.request(frame, 3, &[]);
    fence(server, client);
    let queue = server.state.image_capture_pending.as_ref().unwrap().clone();
    fulfill_test_toplevel_capture(&mut gl.renderer, &mut None, &server.state, &queue);
    let events = fence(server, client);
    assert!(
        events.iter().any(|e| e.sender == frame && e.opcode == 3),
        "real production GL frame not ready"
    );
    assert!(!events.iter().any(|e| e.sender == frame && e.opcode == 4));
    file.seek(SeekFrom::Start(0)).unwrap();
    let mut pixels = vec![0; bytes as usize];
    file.read_exact(&mut pixels).unwrap();
    (frame, pixels, [session, buffer, pool])
}
fn canvas(pixels: &[u8], green: bool) -> bool {
    // Sample only the large drawing area, away from GTK theme/menu/button.
    (100..200).all(|y| {
        (80..240).all(|x| {
            let p = &pixels[((y * W + x) * 4) as usize..][..4];
            p[3] == 255
                && if green {
                    p[1] > 160 && p[0] < 90 && p[2] < 90
                } else {
                    p[0] > 160 && p[1] < 90 && p[2] < 90
                }
        })
    })
}
#[test]
#[ignore = "mandatory dedicated GTK3 CI probe; requires JWM_REMOTE_POPUP_GTK_PROBE executable"]
fn actual_gtk_menu_uses_private_seat_and_activation_changes_captured_pixels() {
    gtk_menu_probe(false);
}

#[test]
#[ignore = "mandatory controlled-startup GTK3 CI probe; requires JWM_REMOTE_POPUP_GTK_PROBE executable"]
fn controlled_startup_gtk_menu_preserves_local_default_and_uses_prepared_seat() {
    gtk_menu_probe(true);
}

fn gtk_menu_probe(controlled: bool) {
    let exe = std::env::var_os("JWM_REMOTE_POPUP_GTK_PROBE")
        .expect("explicit compiled GTK3 probe executable required");
    let mut gl = HeadlessSmithayGles::new().expect("mandatory actual GL renderer required");
    let mut server = Server::new();
    server.state.image_capture_pending = Some(init_image_copy_capture(&server.display.handle()));
    let (native, peer) = UnixStream::pair().unwrap();
    let gtk = server
        .display
        .handle()
        .insert_client(native, Arc::new(JwmClientState::default()))
        .unwrap();
    let gtk_id = gtk.id();
    // Explicit owned launch only, before this connection has dispatched registry.
    let prepared = controlled.then(|| {
        super::prepare_dormant(&mut server.state, gtk_id.clone())
            .expect("explicit dormant seat preparation")
    });
    let mut bystander = server.connect();
    assert!(
        bystander.global_count("wl_seat") == 1,
        "foreign client must see only original local seat"
    );
    let fd = peer.as_raw_fd();
    let mut command = Command::new(exe);
    command
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("JWM_GTK_CONTROLLED_STARTUP")
        .env("GDK_BACKEND", "wayland")
        .env("WAYLAND_SOCKET", fd.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    if controlled {
        command.env("JWM_GTK_CONTROLLED_STARTUP", "1");
    }
    // Only async-signal-safe fcntl in the child; never clear parent CLOEXEC or
    // expose a compositor socket path to an unrelated process.
    unsafe {
        command.pre_exec(move || {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut process = Process(command.spawn().expect("launch real GTK3 probe"));
    drop(peer);
    let stdout = process.0.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    let reader = thread::spawn(move || {
        for (index, line) in BufReader::new(stdout).lines().enumerate() {
            assert!(index < 128, "bounded probe output");
            let line = line.unwrap();
            assert!(line.len() <= 1024);
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let start = Instant::now();
    let deadline = start + Duration::from_secs(20);
    let mut configured = None;
    let root = loop {
        server.roundtrip();
        assert!(Instant::now() < deadline, "GTK root never mapped");
        assert!(
            process.0.try_wait().unwrap().is_none(),
            "GTK exited before mapping"
        );
        let matches: Vec<_> = server
            .state
            .toplevels
            .iter()
            .filter(|(_, top)| top.wl_surface().client().is_some_and(|c| c.id() == gtk_id))
            .map(|(id, _)| *id)
            .collect();
        assert!(matches.len() <= 1, "one exact GTK root");
        if let Some(&id) = matches.first() {
            if configured.is_none() {
                let g = server.state.window_geometry.get_mut(&id).unwrap();
                g.w = W;
                g.h = H;
                server.state.toplevels[&id]
                    .with_pending_state(|pending| pending.size = Some((W as i32, H as i32).into()));
                server.state.toplevels[&id].send_configure();
                configured = Some(id);
            }
            if server.state.mapped_windows.contains(&id) {
                break id;
            }
        }
        thread::yield_now();
    };
    let surface = server.state.surface_for_window(root).unwrap();
    assert_eq!(surface.client().unwrap().id(), gtk_id);
    eprintln!("JWM_GTK_STAGE=root_mapped");
    let mut capture = Capture::new_with_size(&mut server, (W, H));
    let original = capture.source;
    let (_, pixels, _) = frame(&mut server, &mut capture, original, &mut gl);
    assert!(
        canvas(&pixels, false),
        "actual GTK initial blue drawing area"
    );
    eprintln!("JWM_GTK_STAGE=initial_capture");
    if let Some(seat) = &prepared {
        assert!(seat.get_keyboard().is_none());
        let pointer = seat.get_pointer().unwrap();
        assert!(pointer.current_focus().is_none());
        assert!(!pointer.has_pressed_buttons() && !pointer.is_grabbed());
        assert!(!super::active(&server.state));
        // Standard local-seat delivery, not remote authorization or a toolkit
        // activation call. GTK verifies that this device belongs to its default.
        let local = server.state.seat.get_pointer().unwrap();
        local.motion(
            &mut server.state,
            Some((surface.clone(), (0.0, 0.0).into())),
            &MotionEvent {
                location: (100.0, 120.0).into(),
                serial: SERIAL_COUNTER.next_serial(),
                time: InputTime::now(),
            },
        );
        for state in [ButtonState::Pressed, ButtonState::Released] {
            local.button(
                &mut server.state,
                &ButtonEvent {
                    serial: SERIAL_COUNTER.next_serial(),
                    time: InputTime::now(),
                    button: 0x110,
                    state,
                },
            );
        }
        local.frame(&mut server.state);
        let mut local_confirmed = false;
        loop {
            server.roundtrip();
            for line in rx.try_iter() {
                assert!(
                    line != "{\"event\":\"local_default_failed\"}",
                    "GTK local default seat changed"
                );
                local_confirmed |= line == "{\"event\":\"local_default_confirmed\"}";
            }
            callbacks(&surface, start.elapsed().as_millis() as u32);
            let (observed, pixels, [session, buffer, pool]) =
                frame(&mut server, &mut capture, original, &mut gl);
            let orange = pixels
                .chunks_exact(4)
                .enumerate()
                .filter(|(i, _)| {
                    let x = *i as u32 % W;
                    let y = *i as u32 / W;
                    (4..12).contains(&x) && (52..60).contains(&y)
                })
                .all(|(_, p)| p[2] > 220 && p[1] > 100 && p[1] < 160 && p[0] < 30);
            capture.client.request(observed, 0, &[]);
            capture.client.request(session, 1, &[]);
            capture.client.request(buffer, 0, &[]);
            capture.client.request(pool, 1, &[]);
            fence(&mut server, &mut capture.client);
            assert!(
                Instant::now() < deadline,
                "GTK local default input pixel deadline"
            );
            if local_confirmed && orange {
                break;
            }
        }
        assert!(pointer.current_focus().is_none());
        assert!(!pointer.has_pressed_buttons() && !pointer.is_grabbed());
        eprintln!("JWM_GTK_STAGE=local_default_verified");
    }
    let original_local = server.state.seat.get_pointer().unwrap();
    let original_local_focus = original_local.current_focus();
    let original_local_location = original_local.current_location();
    callbacks(&surface, start.elapsed().as_millis() as u32);
    let manager = capture.client.bind("jwm_remote_popup_manager_v1", 1);
    let lease = capture.client.new_id();
    capture.client.request(manager, 1, &[lease, original]);
    let events = fence(&mut server, &mut capture.client);
    let grant = events
        .iter()
        .find(|e| e.sender == lease && e.opcode == 0)
        .expect("explicit popup lease grant");
    let epoch = grant.args[0];
    assert_eq!(grant.args[1], 30_000);
    eprintln!("JWM_GTK_STAGE=lease_granted");
    let family = capture.client.new_id();
    capture.client.request(lease, 1, &[family]);
    fence(&mut server, &mut capture.client);
    loop {
        server.roundtrip();
        let active = server
            .state
            .remote_popup
            .active
            .as_ref()
            .expect("GTK lease revoked before seat binding");
        assert_eq!(
            active.family.root.surface, surface,
            "exact GTK root resource, not only same ClientId"
        );
        if active
            .seat
            .get_pointer()
            .unwrap()
            .snapshot_private_pointer(gtk_id.clone())
            .is_some()
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "GTK did not bind the newly advertised private pointer; no main-seat fallback"
        );
        thread::yield_now();
    }
    eprintln!("JWM_GTK_STAGE=private_pointer_bound");
    let (proof, _, _) = frame(&mut server, &mut capture, family, &mut gl);
    capture.client.request(lease, 2, &[epoch, proof]);
    let events = fence(&mut server, &mut capture.client);
    assert!(
        events
            .iter()
            .any(|e| e.sender == lease && e.opcode == 1 && e.args == [epoch]),
        "actual arm"
    );
    eprintln!("JWM_GTK_STAGE=armed");
    let (proof, _, _) = frame(&mut server, &mut capture, family, &mut gl);
    capture
        .client
        .request(lease, 3, &[epoch, 1, proof, 1, 160, 24]);
    let events = fence(&mut server, &mut capture.client);
    assert!(
        events
            .iter()
            .any(|e| e.sender == lease && e.opcode == 2 && e.args == [epoch, 1])
    );
    eprintln!("JWM_GTK_STAGE=root_clicked");
    let popup = loop {
        server.roundtrip();
        let active = server
            .state
            .remote_popup
            .active
            .as_ref()
            .expect("GTK popup/serial unsupported: native lease revoked");
        if active.grabbed && active.popup_mapped {
            break active
                .family
                .snapshot(&server.state)
                .expect("current family layout")
                .layout
                .popup
                .clone()
                .expect("actual GTK direct popup");
        }
        assert!(
            Instant::now() < deadline,
            "GTK did not create and restricted-grab its menu from the real click"
        );
        thread::yield_now();
    };
    assert_eq!(popup.surface.client().unwrap().id(), gtk_id);
    eprintln!("JWM_GTK_STAGE=popup_mapped");
    let (x, y, w, h) = popup.rect;
    assert!(w > 0 && h > 0);
    let (x, y) = (x + w / 2, y + h / 2);
    assert!(
        x >= 0 && y >= 0 && x < W as i32 && y < H as i32,
        "menu selection must remain inside fixed root capture"
    );
    // Same current toolkit state, root-only capture: button hover/pressed
    // rendering cannot be mistaken for pixels belonging to the popup.
    let (_, root_only, _) = frame(&mut server, &mut capture, original, &mut gl);
    let (proof, popup_pixels, _) = frame(&mut server, &mut capture, family, &mut gl);
    let (left, top, width, height) = popup.rect;
    let right = left.checked_add(width).expect("popup right");
    let bottom = top.checked_add(height).expect("popup bottom");
    assert!(left >= 0 && top >= 0 && right <= W as i32 && bottom <= H as i32);
    let mut changed_inside = 0;
    let mut unchanged_outside = 0;
    for py in 48..H {
        for px in 0..W {
            let offset = ((py * W + px) * 4) as usize;
            let before = &root_only[offset..offset + 4];
            let after = &popup_pixels[offset..offset + 4];
            if (left..right).contains(&(px as i32)) && (top..bottom).contains(&(py as i32)) {
                changed_inside += usize::from(before != after);
            } else if (100..200).contains(&py) && (80..240).contains(&px) {
                assert_eq!(
                    after, before,
                    "canvas outside popup changed before activation at {px},{py}"
                );
                unchanged_outside += 1;
            }
        }
    }
    assert!(
        changed_inside > 0,
        "popup rectangle below the button must contain actual changed pixels"
    );
    assert!(
        unchanged_outside > 0,
        "independent unchanged canvas evidence outside popup required"
    );
    eprintln!("JWM_GTK_STAGE=popup_captured");
    callbacks(&surface, start.elapsed().as_millis() as u32);
    callbacks(&popup.surface, start.elapsed().as_millis() as u32);
    capture
        .client
        .request(lease, 3, &[epoch, 2, proof, 1, x as u32, y as u32]);
    let events = fence(&mut server, &mut capture.client);
    assert!(
        events
            .iter()
            .any(|e| e.sender == lease && e.opcode == 2 && e.args == [epoch, 2]),
        "actual popup selection dispatch"
    );
    eprintln!("JWM_GTK_STAGE=selection_dispatched");
    let mut activation = 0;
    let mut changed = false;
    loop {
        server.roundtrip();
        assert!(Instant::now() < deadline, "activation/pixel deadline");
        for line in rx.try_iter() {
            assert!(
                !line.contains("duplicate_activation") && !line.contains("probe_deadline"),
                "GTK probe reported duplicate activation or deadline"
            );
            if line == "{\"event\":\"menu_activated\",\"count\":1}" {
                activation += 1;
            }
        }
        assert!(
            process.0.try_wait().unwrap().is_none(),
            "GTK exited before activation/pixel evidence completed"
        );
        let (captured, pixels, [session, buffer, pool]) =
            frame(&mut server, &mut capture, original, &mut gl);
        callbacks(&surface, start.elapsed().as_millis() as u32);
        // These post-action observations are not input proofs. Retire every
        // real protocol object so a bounded time wait cannot exhaust sessions.
        capture.client.request(captured, 0, &[]);
        capture.client.request(session, 1, &[]);
        capture.client.request(buffer, 0, &[]);
        capture.client.request(pool, 1, &[]);
        fence(&mut server, &mut capture.client);
        changed |= canvas(&pixels, true);
        assert!(activation <= 1);
        assert!(Instant::now() < deadline, "activation/readback deadline");
        if activation == 1 && changed {
            break;
        }
        thread::yield_now();
    }
    assert_eq!(
        activation, 1,
        "real GTK menu activation, not only native accepted"
    );
    assert!(
        changed,
        "actual production capture must show new green app canvas"
    );
    eprintln!("JWM_GTK_STAGE=activation_verified");
    if let Some(seat) = &prepared {
        let local = server.state.seat.get_pointer().unwrap();
        assert_eq!(local.current_focus(), original_local_focus);
        assert_eq!(local.current_location(), original_local_location);
        assert!(!local.has_pressed_buttons() && !local.is_grabbed());
        capture.client.request(lease, 0, &[]);
        fence(&mut server, &mut capture.client);
        assert!(!super::active(&server.state));
        assert!(seat.get_pointer().is_none() && seat.get_keyboard().is_none());
        assert!(
            !fence(&mut server, &mut bystander)
                .iter()
                .any(|e| e.sender == 2 && e.opcode == 0),
            "retired private seat must never become visible to foreign client"
        );
        eprintln!("JWM_GTK_STAGE=prepared_seat_retired");
    }
    drop(process);
    reader.join().unwrap();
}
