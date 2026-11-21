//! Actual Blender viewport controlled by the registered viewer and authenticated CLI.
use super::*;

const LEFT_ALT: u32 = 56;
const Z: u32 = 44;
const FOUR: u32 = 5;

// Report Blender state without dispatching operators or changing the scene.
const SAMPLE_VIEW: &str = r#"
import bpy
import json
import os
from pathlib import Path

report = Path(os.environ['JWM_BLENDER_REPORT'])
bpy.context.preferences.view.show_splash = False
bpy.context.preferences.inputs.use_mouse_emulate_3_button = True
bpy.context.preferences.inputs.use_mouse_continuous = False

def observe():
    for window in bpy.context.window_manager.windows:
        for area in window.screen.areas:
            if area.type == 'VIEW_3D':
                region = next(region for region in area.regions if region.type == 'WINDOW')
                space = area.spaces.active
                view = space.region_3d
                state = {
                    'viewport': [region.x, window.height - region.y - region.height,
                                 region.width, region.height],
                    'rotation': list(view.view_rotation),
                    'distance': view.view_distance,
                    'shading': space.shading.type,
                }
                pending = report.with_suffix('.tmp')
                pending.write_text(json.dumps(state), encoding='utf-8')
                pending.replace(report)
                return 0.05
    return 0.05

bpy.app.timers.register(observe, first_interval=0.0)
"#;

#[derive(serde::Deserialize)]
struct ViewSample {
    viewport: [u32; 4],
    rotation: [f64; 4],
    distance: f64,
    shading: String,
}

fn view_sample(report: &Path) -> Option<ViewSample> {
    let contents = std::fs::read(report).ok()?;
    serde_json::from_slice(&contents).ok()
}

fn rotation_delta(before: &[f64; 4], after: &[f64; 4]) -> f64 {
    // q and -q describe the same rotation.
    let dot: f64 = before.iter().zip(after).map(|(a, b)| a * b).sum();
    2.0 * dot.abs().clamp(0.0, 1.0).acos()
}

fn viewport_pixels(viewport: [u32; 4]) -> impl Iterator<Item = usize> {
    let [x, y, width, height] = viewport;
    (y..y + height)
        .flat_map(move |row| (x..x + width).map(move |col| ((row * SIZE.0 + col) * 4) as usize))
}

fn lit_mesh_pixels(viewport: [u32; 4], image: &[u8]) -> usize {
    // Sample the central scene away from bright labels and window chrome.
    let [x, y, width, height] = viewport;
    viewport_pixels([x + width / 3, y + height / 3, width / 3, height / 3])
        .filter(|&i| {
            let pixel = &image[i..i + 3];
            pixel.iter().all(|&channel| channel > 110) && pixel[0].abs_diff(pixel[2]) <= 20
        })
        .count()
}

fn changed_viewport_pixels(viewport: [u32; 4], before: &[u8], after: &[u8]) -> usize {
    viewport_pixels(viewport)
        .filter(|&i| {
            before[i..i + 3]
                .iter()
                .zip(&after[i..i + 3])
                .any(|(a, b)| a.abs_diff(*b) > 16)
        })
        .count()
}

#[test]
#[ignore = "requires installed Blender, private DBus, standalone CLI and strict software GL"]
fn actual_blender_remote_orbit_zoom_and_shading_popup() {
    let executable = PathBuf::from(std::env::var_os("JWM_REMOTE_BLENDER_BIN").expect("required"));
    assert!(executable.is_absolute() && executable.is_file());
    assert!(std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some());
    let directory = PrivateDir::new();
    let startup = directory.0.join("viewport-probe.py");
    let report = directory.0.join("blender-view.json");
    std::fs::write(&startup, SAMPLE_VIEW).unwrap();
    let profile = directory.0.join("blender-config");
    let mut gl = HeadlessSmithayGles::new().expect("strict software GL required");
    let mut app = ApplicationSession::start(&mut gl, directory, &executable, 1, true, |command| {
        Process::spawn(
            command
                .args([
                    "--factory-startup",
                    "--disable-autoexec",
                    "--window-geometry",
                    "0",
                    "0",
                    "640",
                    "480",
                    "--python",
                ])
                .arg(&startup)
                .env("BLENDER_USER_RESOURCES", &profile)
                .env("BLENDER_USER_CONFIG", &profile)
                .env("JWM_BLENDER_REPORT", &report)
                .env_remove("EGL_PLATFORM"),
        )
    });
    app.wait(&mut gl, "Blender reports its factory 3D viewport", |_| {
        view_sample(&report).is_some_and(|view| view.shading == "SOLID")
    });
    let baseline = view_sample(&report).unwrap();
    let [x, y, width, height] = baseline.viewport;
    assert!(width > 240 && height > 200);
    assert!(x + width <= SIZE.0 && y + height <= SIZE.1);
    app.wait(
        &mut gl,
        "the lit startup cube is visible in the viewer",
        |app| lit_mesh_pixels(baseline.viewport, &app.presented) > 1000,
    );
    let center = (f64::from(x + width / 2), f64::from(y + height / 2));
    let before_drag = app.presented.clone();
    app.move_pointer(center);
    let ready = app.pointer_proof(&mut gl, None);
    app.key(LEFT_ALT, true);
    app.pointer_proof(&mut gl, Some(ready));
    app.left_button(true);
    app.pump_input(&mut gl);
    for offset in [15.0, 30.0, 55.0, 85.0] {
        app.move_pointer((center.0 + offset, center.1 + offset / 2.0));
        app.pump_input(&mut gl);
    }
    app.wait(
        &mut gl,
        "Alt drag changes the viewport while held and keeps video live",
        |app| {
            view_sample(&report)
                .is_some_and(|view| rotation_delta(&baseline.rotation, &view.rotation) > 0.09)
                && changed_viewport_pixels(baseline.viewport, &before_drag, &app.presented) > 1000
        },
    );
    let orbit_pixels = changed_viewport_pixels(baseline.viewport, &before_drag, &app.presented);
    eprintln!("JWM_BLENDER_ORBIT_PIXELS={orbit_pixels}");
    eprintln!(
        "JWM_BLENDER_ORBIT_RADIANS={}",
        rotation_delta(&baseline.rotation, &view_sample(&report).unwrap().rotation)
    );
    app.left_button(false);
    app.key(LEFT_ALT, false);
    app.pump_input(&mut gl);

    let rotated = view_sample(&report).unwrap();
    let before_zoom = app.presented.clone();
    app.pointer_proof(&mut gl, None);
    app.wheel(-1);
    app.wait(
        &mut gl,
        "wheel zoom changes Blender distance and viewer pixels",
        |app| {
            view_sample(&report).is_some_and(|view| view.distance < rotated.distance * 0.97)
                && changed_viewport_pixels(baseline.viewport, &before_zoom, &app.presented) > 700
        },
    );
    let zoomed = view_sample(&report).unwrap();
    eprintln!(
        "JWM_BLENDER_ZOOM_DISTANCE_BEFORE={} AFTER={}",
        rotated.distance, zoomed.distance
    );

    app.move_pointer(center);
    app.pump_input(&mut gl);
    let before_popup = app.presented.clone();
    app.chord(&mut gl, &[], Z);
    app.wait(
        &mut gl,
        "Blender's shading pie menu appears in the actual viewer",
        |app| {
            view_sample(&report).is_some_and(|view| view.shading == "SOLID")
                && changed_viewport_pixels(baseline.viewport, &before_popup, &app.presented) > 2000
        },
    );
    let menu = app.presented.clone();
    let menu_pixels = changed_viewport_pixels(baseline.viewport, &before_popup, &menu);
    eprintln!("JWM_BLENDER_MENU_PIXELS={menu_pixels}");
    app.chord(&mut gl, &[], FOUR);
    app.wait(
        &mut gl,
        "selecting Wireframe changes Blender shading and the viewer",
        |app| {
            view_sample(&report).is_some_and(|view| view.shading == "WIREFRAME")
                && changed_viewport_pixels(baseline.viewport, &menu, &app.presented) > 2000
        },
    );
    eprintln!("JWM_BLENDER_SHADING=WIREFRAME");
    app.disconnect(&mut gl);
}
