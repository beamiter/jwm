//! Local grants and source switches through the installed host and viewer commands.
use super::*;
use serde_json::{Value, json};

#[cfg(feature = "remote-h264")]
#[path = "host_wayland_gpu_4k_integration.rs"]
mod gpu_4k;

struct Session {
    fixture: CliFixture,
    events: PathBuf,
    control: bool,
}

fn records(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn viewer_stats(viewer: &Process) -> Vec<Value> {
    viewer
        .text()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|report| report["type"] == "jwm_remote_viewer_stats")
        .collect()
}

fn queued_frame_count(reports: &[Value]) -> u64 {
    reports
        .iter()
        .map(|report| {
            report["stages"]["decode_queue_ms"]["observations"]
                .as_u64()
                .unwrap()
        })
        .sum()
}

#[cfg(feature = "remote-h264")]
fn latest_host_stats(fixture: &CliFixture) -> Option<Value> {
    fixture.host.text().lines().rev().find_map(|line| {
        let row = serde_json::from_str::<Value>(line).ok()?;
        (row["type"] == "jwm_remote_host_stats").then_some(row)
    })
}

#[cfg(feature = "remote-h264")]
fn wait_transport_phase(
    session: &mut Session,
    gl: &mut HeadlessSmithayGles,
    name: &str,
    catalog: &Value,
    evidence: impl Fn(&CliFixture, &Value, &Value) -> bool,
) -> (Value, Value) {
    let session_id = &catalog["session_id"];
    let epoch = catalog["source_epoch"]
        .as_str()
        .unwrap()
        .parse::<u64>()
        .unwrap();
    let mut matched = None;
    let mut last_unmatched = None;
    session.fixture.wait(gl, name, |fixture| {
        let Some(host) = latest_host_stats(fixture) else {
            return false;
        };
        let sent = &host["last_sent_frame"];
        if &host["session_id"] != session_id || sent["source_epoch"] != epoch {
            return false;
        }
        let rows = records(&session.events);
        let mut matching_reports = rows.iter().rev().filter_map(|row| {
            let metrics = &row["metrics"];
            let decoded = &metrics["last_decoded_frame"];
            (row["state"] == "viewing"
                && row["catalog"] == *catalog
                && &metrics["session_id"] == session_id
                && decoded["source_epoch"] == epoch)
                .then_some(metrics)
        });
        let latest_viewer = matching_reports.next();
        for viewer in latest_viewer.into_iter().chain(matching_reports) {
            let decoded = &viewer["last_decoded_frame"];
            if [
                "geometry_epoch",
                "source_dimensions",
                "encoded_dimensions",
                "codec",
            ]
            .iter()
            .all(|&key| sent[key] == decoded[key])
                && evidence(fixture, &host, viewer)
            {
                matched = Some((host, viewer.clone()));
                return true;
            }
        }
        // The host and viewer windows are independent. Log each distinct
        // unmatched pair for diagnosis without logging each polling tick.
        let latest_viewer = latest_viewer.unwrap_or(&Value::Null);
        let signature = (
            sent["sequence"].as_u64(),
            latest_viewer["last_decoded_frame"]["sequence"].as_u64(),
        );
        if last_unmatched != Some(signature) {
            last_unmatched = Some(signature);
            eprintln!(
                "JWM_NATIVE_WAYLAND_GPU_WAIT {name}: {}",
                json!({
                    "sent": sent,
                    "host_mode": host["mode"],
                    "host_acks": host["drawn_acks"],
                    "host_outstanding": host["outstanding"],
                    "decoded": latest_viewer["last_decoded_frame"],
                    "viewer_acks": latest_viewer["acked_frames"],
                    "viewer_path": latest_viewer["last_presentation_path"],
                    "viewer_downloads": latest_viewer["hardware_download_ms"]["observations"],
                    "last_dmabuf": fixture.last_dmabuf,
                })
            );
        }
        false
    });
    matched.unwrap()
}

#[cfg(feature = "remote-h264")]
fn native_host_capture(host: &Value) -> bool {
    let capture = &host["wayland_capture"];
    let encode = &host["hardware_encode_stages"];
    host["mode"].as_str().is_some_and(|mode| mode.contains("vaapi-resize/wayland-dmabuf"))
        && host["last_sent_frame"]["codec"] == "H.264"
        && host["h264_frames"].as_u64().unwrap_or(0) > 0
        && host["drawn_acks"].as_u64().unwrap_or(0) > 0
        && encode["native_dmabuf_import_ms"]["observations"].as_u64().unwrap_or(0) > 0
        && encode["rgb_copy_ms"]["observations"] == 0
        // The host emits this object only when it has a CPU observation.
        && capture.is_null()
}

#[cfg(feature = "remote-h264")]
fn native_viewer_presentation(fixture: &CliFixture, viewer: &Value) -> bool {
    fixture.last_dmabuf == Some(true)
        && viewer["last_presentation_path"] == "wayland-vaapi-dmabuf"
        && viewer["native_presented_frames"].as_u64().unwrap_or(0) > 0
        && viewer["native_presented_frames"] == viewer["presented_frames"]
        && viewer["acked_frames"].as_u64().unwrap_or(0) > 0
        && viewer["hardware_download_ms"]["observations"] == 0
}

impl Session {
    fn start(gl: &mut HeadlessSmithayGles) -> Self {
        Self::start_mode(gl, false)
    }

    fn start_mode(gl: &mut HeadlessSmithayGles, control: bool) -> Self {
        Self::start_options(gl, control, false, false)
    }

    fn start_options(
        gl: &mut HeadlessSmithayGles,
        control: bool,
        host_gestures: bool,
        gestures: bool,
    ) -> Self {
        Self::start_configured(gl, control, host_gestures, gestures, |_| {})
    }

    fn start_configured(
        gl: &mut HeadlessSmithayGles,
        control: bool,
        host_gestures: bool,
        gestures: bool,
        configure: impl FnOnce(&mut CliFixture),
    ) -> Self {
        let fixture = CliFixture::start(false, 2, None, None, Some(control), host_gestures);
        Self::start_fixture(gl, fixture, control, gestures, configure)
    }

    fn start_fixture(
        gl: &mut HeadlessSmithayGles,
        fixture: CliFixture,
        control: bool,
        gestures: bool,
        configure: impl FnOnce(&mut CliFixture),
    ) -> Self {
        Self::start_fixture_with_viewer(gl, fixture, control, gestures, configure, |_| {})
    }

    fn start_fixture_with_viewer(
        gl: &mut HeadlessSmithayGles,
        mut fixture: CliFixture,
        control: bool,
        gestures: bool,
        configure: impl FnOnce(&mut CliFixture),
        configure_viewer: impl FnOnce(&mut ProcessCommand),
    ) -> Self {
        configure(&mut fixture);
        fixture.wait(gl, "three locally listed windows", |s| {
            s.host.text().contains("comma-separated")
        });
        let listing = fixture.host.text();
        let listed = |title: &str| {
            listing
                .lines()
                .find_map(|line| {
                    let (index, metadata) = line
                        .strip_prefix("jwm-remote: Wayland window [")?
                        .split_once("]: title=")?;
                    (metadata.split_once(" app_id=")?.0 == title).then_some(index)
                })
                .unwrap()
        };
        let first = listed("jwm-first-owned");
        let second = listed("jwm-second-owned");
        assert_ne!(listed("jwm-unshared"), second);
        writeln!(
            fixture.host.child.stdin.as_mut().unwrap(),
            "{first},{second}"
        )
        .unwrap();
        let mut command = fixture.viewer_command(gl, "session");
        if control {
            fixture.hold_feedback = true;
            command.arg("--control");
        }
        if gestures {
            command.arg("--wayland-gestures");
        }
        configure_viewer(&mut command);
        let events = fixture.directory.0.join("events.jsonl");
        let output = File::create(&events).unwrap();
        fixture.viewer = Some(Process::spawn_with_stdout(&mut command, output.into()));
        Self {
            fixture,
            events,
            control,
        }
    }

    fn presented(
        &mut self,
        gl: &mut HeadlessSmithayGles,
        epoch: &str,
        id: u32,
        blue: bool,
    ) -> Value {
        let mut catalog = None;
        self.fixture
            .wait(gl, "confirmed source and actual GL pixels", |s| {
                let current = records(&self.events)
                    .into_iter()
                    .rev()
                    .find(|row| row["event"] == "snapshot");
                let rendered = s
                    .catalog
                    .as_ref()
                    .unwrap()
                    .last_presented
                    .as_ref()
                    .is_some_and(|(surface, color)| {
                        Some(surface) == s.surface.as_ref() && *color == blue
                    });
                if let Some(current) = current
                    && current["state"] == "viewing"
                    && current["catalog"]["source_epoch"] == epoch
                    && current["catalog"]["source_id"] == id
                    && rendered
                {
                    if self.control {
                        assert_eq!(current["wayland_source_control"], true);
                    } else {
                        assert!(current["wayland_input"].is_null());
                    }
                    assert!(current["input_grabbed"].is_null());
                    catalog = Some(current["catalog"].clone());
                    true
                } else {
                    false
                }
            });
        catalog.unwrap()
    }

    fn send(&mut self, request: Value) {
        let stdin = self
            .fixture
            .viewer
            .as_mut()
            .unwrap()
            .child
            .stdin
            .as_mut()
            .unwrap();
        serde_json::to_writer(&mut *stdin, &request).unwrap();
        stdin.write_all(b"\n").unwrap();
    }

    fn select(&mut self, catalog: &Value, target: u32, request: u32) {
        self.send(json!({"command":"select_source","request":request,"selection":{
            "session_id":catalog["session_id"],"source_epoch":catalog["source_epoch"],"source_id":target
        }}));
    }

    fn request_input(&mut self, catalog: &Value, request: u32) {
        self.send(json!({"command":"request_input","request":request,"selection":{
            "session_id":catalog["session_id"],"source_epoch":catalog["source_epoch"],"source_id":catalog["source_id"]
        }}));
    }

    fn focus_viewer(&mut self, gl: &mut HeadlessSmithayGles) {
        self.focus_viewer_at(gl, (160.0, 120.0));
    }

    fn focus_viewer_at(&mut self, gl: &mut HeadlessSmithayGles, location: (f64, f64)) {
        self.fixture
            .wait(gl, "new viewer role has both input devices", |s| {
                s.surface.as_ref().is_some_and(|surface| {
                    let Some(owner) = surface.client() else {
                        return false;
                    };
                    with_renderer_surface_state(surface, |state| state.buffer().is_some())
                        .unwrap_or(false)
                        && s.local
                            .state
                            .seat
                            .get_keyboard()
                            .unwrap()
                            .client_keyboards(&owner)
                            .count()
                            == 1
                        && s.local
                            .state
                            .seat
                            .get_pointer()
                            .unwrap()
                            .client_pointers(&owner)
                            .count()
                            == 1
                })
            });
        let surface = self.fixture.surface.clone().unwrap();
        self.fixture
            .local
            .state
            .set_keyboard_focus(Some(surface), SERIAL_COUNTER.next_serial());
        self.fixture.pointer_at(location, true);
    }

    fn accepted(&mut self, gl: &mut HeadlessSmithayGles, request: u32, accepted: bool) {
        self.fixture.wait(gl, "selection result", |_| {
            records(&self.events).iter().any(|event| {
                event["event"] == "command_result"
                    && event["request"] == request
                    && event["accepted"] == accepted
            })
        });
    }
}

#[cfg(feature = "remote-h264")]
#[test]
#[ignore = "requires an explicitly selected GPU render node, a VA-API encoder and the actual CLI"]
fn native_cli_wayland_gpu_capture_encodes_and_falls_back_on_same_source() {
    use crate::backend::wayland::state::JwmWaylandState;
    use std::os::unix::fs::MetadataExt;

    let node = PathBuf::from(
        std::env::var_os("JWM_TEST_WAYLAND_GPU_CAPTURE_NODE").expect("GPU render node required"),
    );
    let mut gl =
        HeadlessSmithayGles::on_render_node(&node).expect("real GLES on the selected render node");
    let device = node.metadata().unwrap().rdev();
    let formats: Vec<_> = gl
        .renderer
        .egl_context()
        .dmabuf_render_formats()
        .iter()
        .copied()
        .collect();
    let fixture =
        CliFixture::start_with_host(false, 2, None, None, Some(false), false, |command| {
            command
                .args(["--allow-h264", "--stats-json", "--fps", "20"])
                .env("JWM_REMOTE_GPU_CAPTURE", &node);
        });
    let mut session = Session::start_fixture(&mut gl, fixture, false, false, |f| {
        f.lossy_aspect = Some((4, 3));
        f.native.state.ensure_dmabuf_global_with_feedback(
            &f.native.display.handle(),
            formats,
            [],
            device,
        );
    });
    let first = session.presented(&mut gl, "1", 1, false);
    session.fixture.wait(
        &mut gl,
        "first source is GPU captured, encoded and acknowledged",
        |f| {
            latest_host_stats(f).is_some_and(|stats| {
                native_host_capture(&stats) && stats["last_sent_frame"]["source_epoch"] == 1
            })
        },
    );
    let first_stats = latest_host_stats(&session.fixture).unwrap();
    let first_samples = session.fixture.readback_samples;
    session.select(&first, 2, 1);
    let second = session.presented(&mut gl, "2", 2, true);
    assert_eq!(first["session_id"], second["session_id"]);
    session.fixture.wait(
        &mut gl,
        "second source is also GPU captured and encoded",
        |f| {
            latest_host_stats(f).is_some_and(|stats| {
                native_host_capture(&stats) && stats["last_sent_frame"]["source_epoch"] == 2
            })
        },
    );
    let second_stats = latest_host_stats(&session.fixture).unwrap();
    let second_samples = session.fixture.readback_samples;
    let before_fallback = session.fixture.presentations;
    let before_fallback_sequence = second_stats["last_sent_frame"]["sequence"]
        .as_u64()
        .unwrap();
    let role = session.fixture.surface.clone().unwrap();
    let native = &mut session.fixture.native;
    let removed = native.state.dmabuf_global.take().unwrap();
    native
        .state
        .dmabuf_state
        .destroy_global::<JwmWaylandState>(&native.display.handle(), removed);
    session.fixture.wait(
        &mut gl,
        "SHM fallback presents the same authorized source",
        |f| {
            latest_host_stats(f).is_some_and(|stats| {
                stats["mode"]
                    .as_str()
                    .is_some_and(|mode| mode.contains("wayland-surface/cpu-resize/shm"))
                    && stats["last_sent_frame"]["source_epoch"] == 2
                    && stats["last_sent_frame"]["codec"] == "JPEG"
                    && stats["last_sent_frame"]["sequence"]
                        .as_u64()
                        .is_some_and(|sequence| sequence > before_fallback_sequence)
                    && stats["last_sent_frame"]["geometry_epoch"]
                        == second_stats["last_sent_frame"]["geometry_epoch"]
                    && stats["h264_frames"] == 0
                    && stats["drawn_acks"].as_u64().unwrap_or(0) > 0
                    && stats["sent"] == stats["drawn_acks"]
                    && stats["outstanding"] == 0
                    && f.presentations > before_fallback
            })
        },
    );
    let cpu_stats = latest_host_stats(&session.fixture).unwrap();
    assert_eq!(second, session.presented(&mut gl, "2", 2, true));
    let cpu_samples = session.fixture.readback_samples;
    assert_eq!(session.fixture.surface.as_ref(), Some(&role));
    assert!(
        session
            .fixture
            .host
            .text()
            .contains("GPU capture failed, switching to JPEG on the same source")
    );
    session.send(json!({"command":"disconnect","request":2}));
    session.fixture.wait(
        &mut gl,
        "GPU capture fixture exits normally",
        CliFixture::exited,
    );
    assert!(session.fixture.host.status.unwrap().success());
    assert!(
        session
            .fixture
            .viewer
            .as_ref()
            .unwrap()
            .status
            .unwrap()
            .success()
    );
    for (phase, record, samples) in [
        ("first", first_stats, first_samples),
        ("second", second_stats, second_samples),
        ("fallback", cpu_stats, cpu_samples),
    ] {
        eprintln!("JWM_NATIVE_WAYLAND_GPU_PHASE {phase} {record}");
        eprintln!("JWM_NATIVE_WAYLAND_GPU_PIXELS {phase} {}", json!(samples));
    }
}

#[cfg(feature = "remote-h264")]
#[test]
#[ignore = "requires an explicitly selected GPU render node, VA-API and the actual CLI"]
fn native_cli_wayland_gpu_viewer_presents_and_recovers_from_import_rejection() {
    use crate::backend::wayland::state::JwmWaylandState;
    use std::os::unix::fs::MetadataExt;

    let node = PathBuf::from(
        std::env::var_os("JWM_TEST_WAYLAND_GPU_CAPTURE_NODE").expect("GPU render node required"),
    );
    let mut gl = HeadlessSmithayGles::on_render_node(&node).expect("hardware GLES render node");
    let device = node.metadata().unwrap().rdev();
    let formats: Vec<_> = gl
        .renderer
        .egl_context()
        .dmabuf_render_formats()
        .iter()
        .copied()
        .collect();
    let fixture =
        CliFixture::start_with_host(false, 2, None, None, Some(false), false, |command| {
            command
                .args(["--allow-h264", "--stats-json", "--fps", "20"])
                .env("JWM_REMOTE_GPU_CAPTURE", &node);
        });
    let mut session = Session::start_fixture_with_viewer(
        &mut gl,
        fixture,
        false,
        false,
        |f| {
            f.lossy_aspect = Some((4, 3));
            for server in [&mut f.native, &mut f.local] {
                server.state.ensure_dmabuf_global_with_feedback(
                    &server.display.handle(),
                    formats.iter().copied(),
                    [],
                    device,
                );
            }
        },
        |command| {
            command
                .arg("--stats-json")
                .env("JWM_REMOTE_GPU_PRESENT", &node)
                .env("JWM_REMOTE_READ_AHEAD", "1");
        },
    );
    let first = session.presented(&mut gl, "1", 1, false);
    let native_transport = |fixture: &CliFixture, host: &Value, viewer: &Value| {
        native_host_capture(host) && native_viewer_presentation(fixture, viewer)
    };
    let (first_host, first_metrics) = wait_transport_phase(
        &mut session,
        &mut gl,
        "first source stays on the GPU through capture, encode and presentation",
        &first,
        native_transport,
    );
    let first_samples = session.fixture.readback_samples;
    let first_role = session.fixture.surface.clone();
    let before_resize = session.fixture.dmabuf_presentations;
    let before_resize_host = first_host["last_sent_frame"]["sequence"].as_u64().unwrap();
    let before_resize_viewer = first_metrics["last_decoded_frame"]["sequence"]
        .as_u64()
        .unwrap();
    session.fixture.configure_viewer((481, 320));
    let (resized_host, resized_metrics) = wait_transport_phase(
        &mut session,
        &mut gl,
        "resized letterboxed DMA-BUF stays on the GPU on the same role",
        &first,
        |f, host, viewer| {
            native_transport(f, host, viewer)
                && f.viewport == (481, 320)
                && f.dmabuf_presentations > before_resize
                && f.readback_samples[0] == [0, 0, 0, 255]
                && f.readback_samples[1][2] >= 245
                && host["last_sent_frame"]["sequence"].as_u64().unwrap_or(0) > before_resize_host
                && viewer["last_decoded_frame"]["sequence"]
                    .as_u64()
                    .unwrap_or(0)
                    > before_resize_viewer
        },
    );
    assert_eq!(session.fixture.surface, first_role);
    let resized_samples = session.fixture.readback_samples;
    session.select(&first, 2, 1);
    let second = session.presented(&mut gl, "2", 2, true);
    assert_eq!(first["session_id"], second["session_id"]);
    assert_ne!(session.fixture.surface, first_role);
    let (second_host, second_metrics) = wait_transport_phase(
        &mut session,
        &mut gl,
        "second authorized source stays on the GPU end to end",
        &second,
        native_transport,
    );
    assert_ne!(
        first_host["source_dimensions"],
        second_host["source_dimensions"]
    );
    let second_samples = session.fixture.readback_samples;
    let second_role = session.fixture.surface.clone();
    let second_geometry = &second_host["last_sent_frame"]["geometry_epoch"];
    let before_rejection = session.fixture.presentations;
    let second_sequence = second_metrics["last_decoded_frame"]["sequence"]
        .as_u64()
        .unwrap();
    session.fixture.local.state.reject_test_dmabuf_import = true;
    let (viewer_fallback_host, fallback_metrics) = wait_transport_phase(
        &mut session,
        &mut gl,
        "viewer import rejection falls back to SHM while host capture remains GPU-native",
        &second,
        |f, host, viewer| {
            native_host_capture(host)
                && &host["last_sent_frame"]["geometry_epoch"] == second_geometry
                && f.presentations > before_rejection
                && f.last_dmabuf == Some(false)
                && f.viewer
                    .as_ref()
                    .unwrap()
                    .text()
                    .contains("Wayland compositor rejected the native video buffer")
                && viewer["last_presentation_path"] == "rgb"
                && viewer["acked_frames"].as_u64().unwrap_or(0) > 0
                && viewer["last_decoded_frame"]["sequence"]
                    .as_u64()
                    .unwrap_or(0)
                    > second_sequence
                && viewer["hardware_download_ms"]["observations"]
                    .as_u64()
                    .unwrap_or(0)
                    > 0
        },
    );
    let fallback_samples = session.fixture.readback_samples;
    assert_eq!(session.fixture.surface, second_role);
    assert_eq!(session.presented(&mut gl, "2", 2, true), second);
    let before_withdrawal = session.fixture.presentations;
    let fallback_sequence = viewer_fallback_host["last_sent_frame"]["sequence"]
        .as_u64()
        .unwrap();
    let native = &mut session.fixture.native;
    let removed = native.state.dmabuf_global.take().unwrap();
    native
        .state
        .dmabuf_state
        .destroy_global::<JwmWaylandState>(&native.display.handle(), removed);
    let (capture_fallback_host, capture_fallback_metrics) = wait_transport_phase(
        &mut session,
        &mut gl,
        "withdrawn capture DMA-BUF falls back to SHM and JPEG on the same source",
        &second,
        |f, host, viewer| {
            host["mode"]
                .as_str()
                .is_some_and(|mode| mode.contains("wayland-surface/cpu-resize/shm"))
                && host["last_sent_frame"]["codec"] == "JPEG"
                && &host["last_sent_frame"]["geometry_epoch"] == second_geometry
                && host["last_sent_frame"]["sequence"].as_u64().unwrap_or(0) > fallback_sequence
                && host["wayland_capture"]["readback_bytes"]
                    .as_u64()
                    .unwrap_or(0)
                    > 0
                && host["drawn_acks"].as_u64().unwrap_or(0) > 0
                // The final sent JPEG frame must already be acknowledged.
                // Earlier frames in either reporting window may use H.264.
                && host["sent"] == host["drawn_acks"]
                && host["outstanding"] == 0
                && viewer["acked_frames"].as_u64().unwrap_or(0) > 0
                && viewer["last_presentation_path"] == "rgb"
                && viewer["last_decoded_frame"]["sequence"].as_u64().unwrap_or(0) > fallback_sequence
                && f.presentations > before_withdrawal
                && f.last_dmabuf == Some(false)
        },
    );
    assert_eq!(session.fixture.surface, second_role);
    assert_eq!(session.presented(&mut gl, "2", 2, true), second);
    let capture_fallback_samples = session.fixture.readback_samples;
    assert!(
        session
            .fixture
            .host
            .text()
            .contains("GPU capture failed, switching to JPEG on the same source")
    );
    session.send(json!({"command":"disconnect","request":2}));
    session
        .fixture
        .wait(&mut gl, "native viewer fixture exits", CliFixture::exited);
    assert!(session.fixture.host.status.unwrap().success());
    let viewer = session.fixture.viewer.as_ref().unwrap();
    let terminal = records(&session.events).pop().unwrap();
    assert!(
        viewer.status.unwrap().success(),
        "viewer status={}, state={}, failure={}, stderr={}",
        viewer.status.unwrap(),
        terminal["state"],
        terminal["failure"],
        viewer.text(),
    );
    assert_eq!(terminal["state"], "closed");
    let queued_frames = queued_frame_count(&viewer_stats(viewer));
    assert!(queued_frames > 0);
    eprintln!(
        "JWM_NATIVE_WAYLAND_GPU_VIEWER {}",
        json!({
            "first": first_metrics,
            "resized": resized_metrics,
            "second": second_metrics,
            "fallback": fallback_metrics,
            "capture_fallback": capture_fallback_metrics,
            "host": {
                "first": first_host,
                "resized": resized_host,
                "second": second_host,
                "fallback": viewer_fallback_host,
                "capture_fallback": capture_fallback_host,
            },
            "first_bgrx": first_samples,
            "resized_bgrx": resized_samples,
            "second_bgrx": second_samples,
            "fallback_bgrx": fallback_samples,
            "capture_fallback_bgrx": capture_fallback_samples,
            "queued_frames": queued_frames,
        })
    );
}

fn button_edges(receiver: &Recipient) -> Vec<u32> {
    receiver
        .pointer_actions
        .iter()
        .filter_map(|(opcode, args)| (*opcode == 3).then(|| args[3]))
        .collect()
}

fn wait_gesture_proof(
    session: &mut Session,
    gl: &mut HeadlessSmithayGles,
    after: Option<u64>,
) -> u64 {
    let mut sequence = None;
    session
        .fixture
        .wait(gl, "new acknowledged gesture proof", |_| {
            sequence = records(&session.events)
                .into_iter()
                .rev()
                .find(|row| row["event"] == "snapshot")
                .and_then(|row| row["wayland_gestures"]["ready_frame"].as_u64())
                .filter(|frame| after.is_none_or(|old| *frame > old));
            sequence.is_some()
        });
    sequence.unwrap()
}

fn mixed_scale_outputs(server: &mut Server) {
    use crate::backend::wayland_udev::image_copy_capture::wire_test_client::test_output;
    use smithay::{
        output::{Mode, Scale},
        utils::Rectangle,
    };

    for (name, scale, origin, logical) in [
        ("standard", 1, (0, 0), (800, 600)),
        ("hidpi", 2, (800, -120), (960, 720)),
    ] {
        let output = test_output(name);
        let mode = Mode {
            size: (logical.0 * scale, logical.1 * scale).into(),
            refresh: 60_000,
        };
        output.change_current_state(
            Some(mode),
            None,
            Some(Scale::Integer(scale)),
            Some(origin.into()),
        );
        output.set_preferred(mode);
        output.create_global::<crate::backend::wayland::state::JwmWaylandState>(
            &server.display.handle(),
        );
        server
            .state
            .output_rects
            .push(Rectangle::new(origin.into(), logical.into()));
        server.state.outputs.push(output);
    }
}

fn focus_native_window(server: &mut Server, app: &App) {
    let geometry = server.state.window_geometry[&app.window];
    let location = (f64::from(geometry.x) + 5.0, f64::from(geometry.y) + 5.0).into();
    let (window, surface, origin) = server.state.surface_under(location).unwrap();
    assert_eq!(window, Some(app.window));
    assert_eq!(
        surface,
        server.state.surface_for_window(app.window).unwrap()
    );
    server
        .state
        .set_keyboard_focus(Some(surface.clone()), SERIAL_COUNTER.next_serial());
    server.state.pointer_location = location;
    let pointer = server.state.seat.get_pointer().unwrap();
    pointer.motion(
        &mut server.state,
        Some((surface, origin)),
        &MotionEvent {
            location,
            serial: SERIAL_COUNTER.next_serial(),
            time: InputTime::now(),
        },
    );
    pointer.frame(&mut server.state);
    server.roundtrip();
}

#[test]
#[ignore = "requires independent CLI artifact, private Unix listeners and strict GL; run dedicated script"]
fn native_cli_wayland_mixed_scale_outputs_keep_capture_and_control_window_scoped() {
    use crate::backend::wayland::state::JwmWaylandState;
    use smithay::reexports::wayland_server::Resource;
    use smithay::wayland::compositor::CompositorState;

    let mut gl = HeadlessSmithayGles::new().expect("strict headless GL required");
    let mut session = Session::start_configured(&mut gl, true, true, true, |f| {
        // The owned viewer display has no clients yet. Advertise wl_surface
        // v6 to exercise preference events as well as output membership.
        let display = f.local.display.handle();
        display
            .remove_global::<JwmWaylandState>(f.local.state.compositor_state.compositor_global());
        display.remove_global::<JwmWaylandState>(
            f.local.state.compositor_state.subcompositor_global(),
        );
        f.local.state.compositor_state = CompositorState::new_v6::<JwmWaylandState>(&display);
        mixed_scale_outputs(&mut f.native);
        mixed_scale_outputs(&mut f.local);
        let catalog = f.catalog.as_mut().unwrap();
        let blue = &mut catalog.second;
        let geometry = f
            .native
            .state
            .window_geometry
            .get_mut(&blue.window)
            .unwrap();
        (geometry.x, geometry.y) = (840, -80);
        let (buffer, _file) = shm(&mut f.native, &mut blue.client, [255, 0, 0, 255], 480, 360);
        blue.client.request(blue.surface, 8, &[2]); // set_buffer_scale
        blue.client.request(blue.surface, 1, &[buffer, 0, 0]);
        blue.client.request(blue.surface, 6, &[]);
        let unshared = f
            .native
            .state
            .window_geometry
            .get_mut(&catalog._unshared.window)
            .unwrap();
        (unshared.x, unshared.y) = (900, -35);
        f.native.roundtrip();
        f.native.roundtrip();
        let root = f.native.state.surface_for_window(blue.window).unwrap();
        f.native.state.outputs[1].enter(&root);
        assert_eq!(
            with_renderer_surface_state(&root, |state| {
                (state.buffer_scale(), state.buffer_size().unwrap())
            }),
            Some((2, (240, 180).into()))
        );
        focus_native_window(&mut f.native, &f.target);
    });

    session.focus_viewer(&mut gl);
    let role = session.fixture.surface.clone().unwrap();
    session.fixture.local.state.outputs[0].enter(&role);
    session.fixture.hold_feedback = false;
    let first = session.presented(&mut gl, "1", 1, false);
    let first_ready = wait_gesture_proof(&mut session, &mut gl, None);
    let scale_of = |surface| with_renderer_surface_state(surface, |state| state.buffer_scale());
    assert_eq!(scale_of(&role), Some(1));

    session.fixture.key(42, true);
    session
        .fixture
        .wait(&mut gl, "Shift on the first output's source", |f| {
            f.received.keys == [(42, 1)]
        });
    let first_ready = wait_gesture_proof(&mut session, &mut gl, Some(first_ready));
    session.fixture.left_button(true);
    session
        .fixture
        .wait(&mut gl, "first output drag starts", |f| {
            button_edges(&f.received) == [1]
        });
    let presentation = session.fixture.presentations;
    let local = &mut session.fixture.local;
    let window = *local.state.toplevels.keys().next().unwrap();
    let geometry = local.state.window_geometry.get_mut(&window).unwrap();
    (geometry.x, geometry.y) = (840, -80);
    local.state.outputs[1].enter(&role);
    local.state.outputs[0].leave(&role);
    session.fixture.wait(
        &mut gl,
        "same viewer role renders 2× and ends the old drag",
        |f| {
            f.presentations > presentation
                && scale_of(&role) == Some(2)
                && button_edges(&f.received) == [1, 0]
                && f.catalog.as_ref().unwrap().last_presented == Some((role.clone(), false))
        },
    );
    assert_eq!(
        with_renderer_surface_state(&role, |state| state.buffer_size()),
        Some(Some((400, 300).into()))
    );
    assert_eq!(session.fixture.received.keys, [(42, 1)]);
    assert!(remote_control::active(&session.fixture.native.state));
    session.fixture.left_button(false);
    session.fixture.pointer_at((1000.0, 40.0), true);
    wait_gesture_proof(&mut session, &mut gl, Some(first_ready));

    session.select(&first, 2, 31);
    let second = session.presented(&mut gl, "2", 2, true);
    assert_eq!(first["session_id"], second["session_id"]);
    assert_eq!(session.fixture.received.keys, [(42, 1), (42, 0)]);
    assert_eq!(button_edges(&session.fixture.received), [1, 0]);
    assert!(!remote_control::active(&session.fixture.native.state));
    session.fixture.key(42, false);

    let catalog = session.fixture.catalog.as_mut().unwrap();
    let native = &mut session.fixture.native;
    let (blue_pointer, blue_keys) = bind_inputs(native, &mut catalog.second, 7);
    let (other_pointer, other_keys) = bind_inputs(native, &mut catalog._unshared, 7);
    focus_native_window(native, &catalog.second);
    catalog.second.client.events();
    // The remote point is underneath the unshared green window on the host
    // desktop. It must remain a point on the shared blue window.
    assert_eq!(
        native.state.surface_under((936.0, -8.0).into()).unwrap().0,
        Some(catalog._unshared.window)
    );
    let role = session.fixture.surface.clone().unwrap();
    let local = &mut session.fixture.local;
    let window = *local.state.toplevels.keys().next().unwrap();
    let geometry = local.state.window_geometry.get_mut(&window).unwrap();
    (geometry.x, geometry.y) = (840, -80);
    local.state.outputs[1].enter(&role);
    session
        .fixture
        .wait(&mut gl, "second role uses the 2× output scale", |_| {
            scale_of(&role) == Some(2)
        });
    assert!(role.version() >= 6);
    role.preferred_buffer_scale(1);
    session.fixture.wait(
        &mut gl,
        "surface preference overrides the 2× output scale",
        |_| scale_of(&role) == Some(1),
    );
    role.preferred_buffer_scale(2);
    session.fixture.wait(
        &mut gl,
        "second source has a 2× presentation buffer",
        |_| scale_of(&role) == Some(2),
    );
    session.request_input(&second, 32);
    session.accepted(&mut gl, 32, true);
    session.focus_viewer_at(&mut gl, (1000.0, 40.0));
    wait_gesture_proof(&mut session, &mut gl, None);
    session.fixture.left_button(true);
    session.fixture.left_button(false);
    let mut input = Recipient::default();
    session
        .fixture
        .wait(&mut gl, "2× source receives a logical click", |f| {
            input.collect(
                &mut f.catalog.as_mut().unwrap().second,
                blue_pointer,
                blue_keys,
            );
            button_edges(&input) == [1, 0]
        });
    let motion = input
        .pointer_actions
        .iter()
        .find(|(op, _)| *op == 2)
        .unwrap();
    assert_eq!(&motion.1[1..], &[96 * 256, 72 * 256]);
    let mut unrelated = Recipient::default();
    unrelated.collect(
        &mut session.fixture.catalog.as_mut().unwrap()._unshared,
        other_pointer,
        other_keys,
    );
    unrelated.assert_no_input_for("overlapping unshared window");
    assert_eq!(button_edges(&session.fixture.received), [1, 0]);

    session.send(json!({"command":"disconnect","request":33}));
    session.fixture.wait(
        &mut gl,
        "mixed-output session exits normally",
        CliFixture::exited,
    );
    assert!(session.fixture.host.status.unwrap().success());
    assert!(
        session
            .fixture
            .viewer
            .as_ref()
            .unwrap()
            .status
            .unwrap()
            .success()
    );
    eprintln!(
        "JWM_MIXED_OUTPUTS=1x,2x VIEWER_LOGICAL=400x300 VIEWER_HIDPI_BUFFER=800x600 SOURCE_LOGICAL_CLICK=96,72"
    );
    eprintln!("JWM_PREFERRED_SCALE_OVERRIDE=2x,1x,2x");
}

#[test]
#[ignore = "requires independent CLI artifact, private Unix listeners and strict GL; run dedicated script"]
fn native_cli_wayland_gestures_hold_through_video_release_on_resize_and_switch() {
    let mut gl = HeadlessSmithayGles::new().expect("strict headless GL required");
    let mut session = Session::start_options(&mut gl, true, true, true);
    session.focus_viewer(&mut gl);
    session.fixture.hold_feedback = false;
    let first = session.presented(&mut gl, "1", 1, false);
    let proof = wait_gesture_proof(&mut session, &mut gl, None);
    session.fixture.key(42, true);
    session
        .fixture
        .wait(&mut gl, "Shift delivered to red window", |f| {
            f.received.keys == [(42, 1)]
        });
    let proof = wait_gesture_proof(&mut session, &mut gl, Some(proof));

    session.fixture.left_button(true);
    session
        .fixture
        .wait(&mut gl, "button press on red window", |f| {
            button_edges(&f.received) == [1]
        });
    let drag_start_presentation = session.fixture.presentations;
    session
        .fixture
        .wait(&mut gl, "video continues while button is held", |f| {
            f.presentations >= drag_start_presentation + 2
        });
    session.fixture.pointer_at((192.0, 140.0), true);
    session.fixture.pointer_at((216.0, 160.0), true);
    session.fixture.left_button(false);
    session
        .fixture
        .wait(&mut gl, "last drag position precedes button release", |f| {
            button_edges(&f.received) == [1, 0]
        });
    let motion = session
        .fixture
        .received
        .pointer_actions
        .iter()
        .rev()
        .nth(1)
        .unwrap();
    assert_eq!(
        (motion.0, motion.1[1], motion.1[2]),
        (2, 108 * 256, 80 * 256)
    );

    let proof = wait_gesture_proof(&mut session, &mut gl, Some(proof));
    session.fixture.left_button(true);
    session.fixture.wait(&mut gl, "second press", |f| {
        button_edges(&f.received) == [1, 0, 1]
    });
    session.fixture.configure_viewer((320, 240));
    session
        .fixture
        .wait(&mut gl, "resize releases old button", |f| {
            button_edges(&f.received) == [1, 0, 1, 0]
        });
    assert_eq!(session.fixture.received.keys, [(42, 1)]);
    session.fixture.left_button(false);
    wait_gesture_proof(&mut session, &mut gl, Some(proof));

    session.fixture.left_button(true);
    session.fixture.wait(&mut gl, "third press", |f| {
        button_edges(&f.received) == [1, 0, 1, 0, 1]
    });
    session.select(&first, 2, 1);
    let second = session.presented(&mut gl, "2", 2, true);
    assert_eq!(first["session_id"], second["session_id"]);
    assert_eq!(button_edges(&session.fixture.received), [1, 0, 1, 0, 1, 0]);
    assert_eq!(session.fixture.received.keys, [(42, 1), (42, 0)]);
    assert_eq!(latest_input_state(&session.events), "available");
    assert!(!remote_control::active(&session.fixture.native.state));
    session.fixture.left_button(false);
    session.fixture.key(42, false);

    let app = &mut session.fixture.catalog.as_mut().unwrap().second;
    let (pointer, keyboard) = bind_inputs(&mut session.fixture.native, app, 7);
    focus(&mut session.fixture.native, app);
    app.client.events();
    session.fixture.hold_feedback = true;
    session.fixture.configure_viewer((360, 270));
    session
        .fixture
        .wait(&mut gl, "host resized before the new input request", |f| {
            f.host.text().contains("encoding to fit 360x270")
        });
    session.request_input(&second, 2);
    session.accepted(&mut gl, 2, true);
    session.focus_viewer(&mut gl);
    session.fixture.wait(
        &mut gl,
        "input request is queued before replacement presentation",
        |f| {
            f.host
                .text()
                .contains("queued managed input preparation for Wayland source epoch 2")
        },
    );
    assert!(!remote_control::active(&session.fixture.native.state));
    session.fixture.hold_feedback = false;
    wait_gesture_proof(&mut session, &mut gl, None);
    session.fixture.left_button(true);
    let mut blue_input = Recipient::default();
    session
        .fixture
        .wait(&mut gl, "reauthorized blue window receives press", |f| {
            blue_input.collect(&mut f.catalog.as_mut().unwrap().second, pointer, keyboard);
            button_edges(&blue_input) == [1]
        });
    session.fixture.pointer(false);
    session
        .fixture
        .wait(&mut gl, "leaving the image balances the blue drag", |f| {
            blue_input.collect(&mut f.catalog.as_mut().unwrap().second, pointer, keyboard);
            button_edges(&blue_input) == [1, 0]
        });
    assert_eq!(button_edges(&session.fixture.received), [1, 0, 1, 0, 1, 0]);
    session.fixture.left_button(false);
    session.send(json!({"command":"disconnect","request":3}));
    session.fixture.wait(
        &mut gl,
        "gesture session exits normally",
        CliFixture::exited,
    );
    assert!(session.fixture.host.status.unwrap().success());
    assert!(
        session
            .fixture
            .viewer
            .as_ref()
            .unwrap()
            .status
            .unwrap()
            .success()
    );
    eprintln!(
        "Wayland gestures: modifier drag, continued video, resize cancellation, confirmed source switch and explicit gesture regrant"
    );
}

#[test]
#[ignore = "requires independent CLI artifact, private Unix listeners and strict GL; run dedicated script"]
fn native_cli_wayland_gestures_requires_authenticated_and_native_opt_in() {
    let mut gl = HeadlessSmithayGles::new().expect("strict headless GL required");
    for (host_opt_in, version, expected, category) in [
        (
            false,
            6,
            "Wayland gestures require explicit host permission",
            "wayland_gestures_permission_required",
        ),
        (
            true,
            5,
            "Wayland gestures require native source control version 6",
            "native_wayland_gestures_unavailable",
        ),
    ] {
        let mut session = remote_control::with_manager_version_for_test(version, || {
            Session::start_options(&mut gl, true, host_opt_in, true)
        });
        session.fixture.wait(
            &mut gl,
            "both helpers reject a missing gesture grant",
            CliFixture::exited,
        );
        assert!(!session.fixture.host.status.unwrap().success());
        let viewer = session.fixture.viewer.as_ref().unwrap();
        assert!(!viewer.status.unwrap().success());
        assert!(
            session.fixture.host.text().contains(expected),
            "{}",
            session.fixture.host.text()
        );
        assert!(
            records(&session.events)
                .iter()
                .any(|row| row["state"] == "failed"
                    && row["failure"]["stage"] == "negotiate"
                    && row["failure"]["category"] == category
                    && row["failure"]["peer_close_code"] == "capture_unavailable")
        );
        assert_eq!(session.fixture.presentations, 0);
        assert_eq!(
            remote_control::attempts_for_test(&session.fixture.native.state),
            0
        );
    }
}

fn latest_input_state(events: &Path) -> Value {
    records(events)
        .into_iter()
        .rev()
        .find(|row| row["event"] == "snapshot")
        .map(|row| row["wayland_input"].clone())
        .unwrap_or(Value::Null)
}

#[test]
#[ignore = "requires independent CLI artifact, private Unix listeners and strict GL; run dedicated script"]
fn native_cli_switch_releases_old_grant_and_explicitly_controls_presented_target() {
    let mut gl = HeadlessSmithayGles::new().expect("strict headless GL required");
    let mut session = Session::start_mode(&mut gl, true);
    session.focus_viewer(&mut gl);
    session.fixture.hold_feedback = false;
    let first = session.presented(&mut gl, "1", 1, false);
    session
        .fixture
        .wait(&mut gl, "first native input active", |s| {
            latest_input_state(&session.events) == "active"
                && remote_control::active(&s.native.state)
        });
    let epoch = remote_control::cli_lease_snapshot_for_test(&session.fixture.native.state)
        .unwrap()
        .0;
    let original_role = session.fixture.surface.clone().unwrap();
    session.fixture.key(42, true);
    session
        .fixture
        .wait(&mut gl, "first target holds Shift", |s| {
            s.received.keys == [(42, 1)]
        });
    session.select(&first, 2, 1);

    // Deliver local messages while the host compositor cannot complete its fence.
    let deadline = Instant::now() + Duration::from_secs(3);
    while latest_input_state(&session.events) != "releasing" {
        session.fixture.local.roundtrip();
        assert!(
            Instant::now() < deadline,
            "source selection did not begin input cleanup"
        );
        thread::sleep(Duration::from_millis(1));
    }
    assert!(remote_control::active(&session.fixture.native.state));
    assert!(original_role.is_alive());
    assert_eq!(session.fixture.received.keys, [(42, 1)]);
    let in_transition = records(&session.events)
        .into_iter()
        .rev()
        .find(|row| row["event"] == "snapshot")
        .unwrap();
    assert!(in_transition["catalog"].is_null());
    session.select(&first, 2, 7);
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(result) = records(&session.events)
            .into_iter()
            .find(|row| row["event"] == "command_result" && row["request"] == 7)
        {
            assert_eq!(result["accepted"], false);
            break;
        }
        session.fixture.local.roundtrip();
        assert!(
            Instant::now() < deadline,
            "second source request not rejected"
        );
        thread::sleep(Duration::from_millis(1));
    }

    let second = session.presented(&mut gl, "2", 2, true);
    assert_eq!(first["session_id"], second["session_id"]);
    assert!(!original_role.is_alive());
    assert_eq!(session.fixture.received.keys, [(42, 1), (42, 0)]);
    assert!(!remote_control::active(&session.fixture.native.state));
    assert_eq!(
        remote_control::attempts_for_test(&session.fixture.native.state),
        1
    );
    assert_eq!(latest_input_state(&session.events), "available");
    session.fixture.key(42, false);

    let app = &mut session.fixture.catalog.as_mut().unwrap().second;
    let (pointer, keyboard) = bind_inputs(&mut session.fixture.native, app, 7);
    focus(&mut session.fixture.native, app);
    app.client.events();
    session.request_input(&first, 2);
    session.accepted(&mut gl, 2, false);
    assert!(!remote_control::active(&session.fixture.native.state));

    session.request_input(&second, 3);
    session.accepted(&mut gl, 3, true);
    session.focus_viewer(&mut gl);
    session
        .fixture
        .wait(&mut gl, "explicit replacement native input active", |s| {
            latest_input_state(&session.events) == "active"
                && remote_control::active(&s.native.state)
        });
    let new_epoch = remote_control::cli_lease_snapshot_for_test(&session.fixture.native.state)
        .unwrap()
        .0;
    assert_ne!(new_epoch, epoch);
    session.fixture.key(30, true);
    session.fixture.key(30, false);
    let mut received = Recipient::default();
    session
        .fixture
        .wait(&mut gl, "only replacement target receives A", |s| {
            received.collect(&mut s.catalog.as_mut().unwrap().second, pointer, keyboard);
            received.keys == [(30, 1), (30, 0)]
        });
    assert_eq!(session.fixture.received.keys, [(42, 1), (42, 0)]);
    session.select(&second, 1, 4);
    let returned = session.presented(&mut gl, "3", 1, false);
    assert_eq!(latest_input_state(&session.events), "unavailable");
    session.request_input(&returned, 5);
    session.accepted(&mut gl, 5, false);
    assert_eq!(
        remote_control::attempts_for_test(&session.fixture.native.state),
        2
    );
    assert!(!remote_control::active(&session.fixture.native.state));
    session.send(json!({"command":"disconnect","request":6}));
    session.fixture.wait(
        &mut gl,
        "session closes after both grants",
        CliFixture::exited,
    );
    assert!(session.fixture.host.status.unwrap().success());
    assert!(
        session
            .fixture
            .viewer
            .as_ref()
            .unwrap()
            .status
            .unwrap()
            .success()
    );
    eprintln!(
        "managed Wayland switch: Shift released on the first target, A delivered only to the explicitly reauthorized second target"
    );
}

#[test]
#[ignore = "requires independent CLI artifact, private Unix listeners and strict GL; run dedicated script"]
fn native_cli_source_catalog_rebuilds_wayland_roles_and_retires_old_feedback() {
    for read_ahead in [false, true] {
        exercise_native_cli_source_catalog(read_ahead);
    }
}

fn exercise_native_cli_source_catalog(read_ahead: bool) {
    let mut gl = HeadlessSmithayGles::new().expect("strict headless GL required");
    let fixture = CliFixture::start(false, 2, None, None, Some(false), false);
    let mut session = Session::start_fixture_with_viewer(
        &mut gl,
        fixture,
        false,
        false,
        |_| {},
        |command| {
            command.arg("--stats-json");
            command.env("JWM_REMOTE_READ_AHEAD", if read_ahead { "1" } else { "0" });
        },
    );
    let first = session.presented(&mut gl, "1", 1, false);
    assert_eq!(
        first["entries"],
        json!([
            {"source_id":1,"label":"Authorized Wayland window 1"},
            {"source_id":2,"label":"Authorized Wayland window 2"}
        ])
    );
    let old = session.fixture.surface.clone().unwrap();
    session.select(&first, 3, 1);
    session.accepted(&mut gl, 1, false);

    session.fixture.hold_feedback = true;
    let mut retiring = Vec::new();
    session
        .fixture
        .wait(&mut gl, "unacknowledged old-source commit", |_| {
            retiring = take_feedback(&old);
            !retiring.is_empty()
        });
    session.select(&first, 2, 2);
    session.accepted(&mut gl, 2, true);
    session
        .fixture
        .wait(&mut gl, "new xdg role awaits first presentation", |s| {
            !old.is_alive()
                && s.surface.as_ref().is_some_and(|current| {
                    current != &old
                        && with_renderer_surface_state(current, |state| state.buffer().is_some())
                            .unwrap_or(false)
                })
        });
    let replacement = session.fixture.surface.clone().unwrap();
    let mut readback = Readback::new(&mut session.fixture.local, session.fixture.viewport);
    let pixels = readback
        .sample(&mut session.fixture.local, &mut gl)
        .expect("read back replacement source with feedback withheld");
    assert!(replacement.is_alive());
    assert!(
        pixels
            .chunks_exact(4)
            .all(|p| { p[0] >= 250 && p[1] <= 3 && p[2] <= 3 && p[3] == 255 })
    );
    // Delayed feedback on the retired role cannot confirm the replacement.
    present(&mut session.fixture.local, vec![retiring.remove(0)]);
    for callback in retiring {
        callback.discarded();
    }
    session.fixture.wait(
        &mut gl,
        "no current source while presentation is withheld",
        |_| {
            records(&session.events)
                .into_iter()
                .rev()
                .find(|r| r["event"] == "snapshot")
                .is_some_and(|r| r["state"] == "waiting_for_presentation" && r["catalog"].is_null())
        },
    );
    session.fixture.hold_feedback = false;
    let second = session.presented(&mut gl, "2", 2, true);
    let second_surface = session.fixture.surface.clone().unwrap();
    assert_eq!(second["session_id"], first["session_id"]);
    session.select(&first, 1, 3);
    session.accepted(&mut gl, 3, false);
    session.select(&second, 1, 4);
    session.accepted(&mut gl, 4, true);
    let returned = session.presented(&mut gl, "3", 1, false);
    assert_eq!(returned["session_id"], first["session_id"]);
    assert!(!second_surface.is_alive());
    assert_eq!(session.fixture.local.state.toplevels.len(), 1);
    session.send(json!({"command":"disconnect","request":5}));
    session
        .fixture
        .wait(&mut gl, "clean source session exit", CliFixture::exited);
    assert!(session.fixture.host.status.unwrap().success());
    let viewer = session.fixture.viewer.as_ref().unwrap();
    assert!(viewer.status.unwrap().success());
    let reports = viewer_stats(viewer);
    assert!(
        reports
            .iter()
            .any(|report| report["last_frame"]["source_epoch"] == 3)
    );
    let queued_frames = queued_frame_count(&reports);
    assert_eq!(queued_frames > 0, read_ahead);
    eprintln!(
        "native Wayland sources: two grants, red to blue to red, epochs 1–3, one TCP session, read-ahead={read_ahead}, queued frames={queued_frames}"
    );
}

#[test]
#[ignore = "requires independent CLI artifact, private Unix listeners and strict GL; run dedicated script"]
fn native_cli_source_catalog_never_reopens_a_remapped_inactive_grant() {
    let mut gl = HeadlessSmithayGles::new().expect("strict headless GL required");
    let mut session = Session::start(&mut gl);
    let first = session.presented(&mut gl, "1", 1, false);
    let catalog = session.fixture.catalog.as_mut().unwrap();
    catalog.second.unmap(&mut session.fixture.native);
    catalog.second.remap(&mut session.fixture.native);
    session.select(&first, 2, 1);
    session.accepted(&mut gl, 1, true);
    session.fixture.wait(
        &mut gl,
        "original target authority refuses remapped surface",
        CliFixture::exited,
    );
    assert!(!session.fixture.host.status.unwrap().success());
    assert!(
        !session
            .fixture
            .viewer
            .as_ref()
            .unwrap()
            .status
            .unwrap()
            .success()
    );
    assert!(
        !records(&session.events)
            .iter()
            .any(|r| r["catalog"]["source_epoch"] == "2")
    );
    assert_eq!(
        session
            .fixture
            .catalog
            .as_ref()
            .unwrap()
            .last_presented
            .as_ref()
            .unwrap()
            .1,
        false
    );
    let host = session.fixture.host.text();
    assert!(
        host.contains("selected Wayland toplevel is no longer live")
            || host.contains("Wayland image capture session stopped"),
        "{host}"
    );
    eprintln!(
        "native Wayland sources: remapping the inactive target did not revive its original grant"
    );
}
