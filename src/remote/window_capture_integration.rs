//! Real-server tests for the experimental window backend.
//!
//! Run only on a fresh, dedicated 24-bit Xvfb server, never the user's desktop:
//! `JWM_REMOTE_TEST_DISPLAY=:99 cargo test --features remote-x11 \
//!     window_capture_integration -- --ignored --test-threads=1`
//! An explicit display is mandatory; no fallback to DISPLAY is allowed. The
//! fixture also refuses any existing windows or compositor before acquiring its
//! private compositor selection. These tests prove X11/software behavior, not
//! physical latency, GPU-driver behavior, or dual-machine deployment readiness.

use super::window_capture::WindowCapture;
use super::x11_capture::{CaptureImageRecycler, CaptureOutcome, decode_ximage_reusing};
use image::RgbImage;
use std::cell::RefCell;
use std::sync::{Mutex, MutexGuard};
use x11rb::connection::Connection;
use x11rb::image::{Image as XImage, PixelLayout};
use x11rb::protocol::composite::{self, Redirect};
use x11rb::protocol::xproto::{
    ColormapAlloc, ConfigureWindowAux, ConnectionExt as _, CreateGCAux, CreateWindowAux,
    ImageFormat, Rectangle, VisualClass, Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

const WIDTH: u16 = 64;
const HEIGHT: u16 = 48;
static TEST_SERVER_LOCK: Mutex<()> = Mutex::new(());

struct Fixture {
    // Fields drop in declaration order: release all X resources before allowing
    // another test to check that the dedicated server has no existing windows.
    conn: RustConnection,
    display: String,
    root: Window,
    screen_num: usize,
    root_windows: RefCell<Vec<Window>>,
    _serial: MutexGuard<'static, ()>,
}

impl Fixture {
    fn new() -> Self {
        let serial = TEST_SERVER_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let display = std::env::var("JWM_REMOTE_TEST_DISPLAY")
            .expect("set JWM_REMOTE_TEST_DISPLAY to a dedicated, empty Xvfb server");
        assert!(!display.is_empty(), "test display must not be empty");
        let (conn, screen_num) = x11rb::connect(Some(&display)).expect("connect to test Xvfb");
        let screen = &conn.setup().roots[screen_num];
        let root = screen.root;
        assert_eq!(screen.root_depth, 24, "use a 24-bit Xvfb screen");
        let visual = screen
            .allowed_depths
            .iter()
            .flat_map(|d| &d.visuals)
            .find(|v| v.visual_id == screen.root_visual)
            .unwrap();
        assert_eq!(
            (visual.red_mask, visual.green_mask, visual.blue_mask),
            (0xff0000, 0x00ff00, 0x0000ff),
            "fixture colors require standard TrueColor masks"
        );
        let selection = conn
            .intern_atom(false, format!("_NET_WM_CM_S{screen_num}").as_bytes())
            .unwrap()
            .reply()
            .unwrap()
            .atom;
        assert_eq!(
            conn.get_selection_owner(selection)
                .unwrap()
                .reply()
                .unwrap()
                .owner,
            0,
            "refusing to replace an existing compositor"
        );
        assert!(
            conn.query_tree(root)
                .unwrap()
                .reply()
                .unwrap()
                .children
                .is_empty(),
            "refusing a display containing existing windows; use a fresh dedicated Xvfb"
        );
        let version = composite::query_version(&conn, 0, 4)
            .unwrap()
            .reply()
            .unwrap();
        assert!((version.major_version, version.minor_version) >= (0, 2));
        let owner = conn.generate_id().unwrap();
        conn.create_window(
            0,
            owner,
            root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_OUTPUT,
            0,
            &CreateWindowAux::new(),
        )
        .unwrap()
        .check()
        .unwrap();
        conn.set_selection_owner(owner, selection, x11rb::CURRENT_TIME)
            .unwrap()
            .check()
            .unwrap();
        conn.sync().unwrap();
        Self {
            conn,
            display,
            root,
            screen_num,
            root_windows: RefCell::new(vec![owner]),
            _serial: serial,
        }
    }

    fn create(&self, id: Window, parent: Window, x: i16, y: i16, color: u32) {
        self.conn
            .create_window(
                0,
                id,
                parent,
                x,
                y,
                WIDTH,
                HEIGHT,
                0,
                WindowClass::INPUT_OUTPUT,
                0,
                &CreateWindowAux::new()
                    .background_pixel(color)
                    .override_redirect(1),
            )
            .unwrap()
            .check()
            .unwrap();
        if parent == self.root {
            self.root_windows.borrow_mut().push(id);
        }
    }

    fn plain_window(&self, parent: Window, x: i16, y: i16, color: u32) -> Window {
        let id = self.conn.generate_id().unwrap();
        self.create(id, parent, x, y, color);
        self.conn.map_window(id).unwrap().check().unwrap();
        self.conn.sync().unwrap();
        id
    }

    fn patterned_target(&self, parent: Window, x: i16, y: i16) -> Window {
        let id = self.conn.generate_id().unwrap();
        self.create_patterned_target(id, parent, x, y);
        id
    }

    fn create_patterned_target(&self, id: Window, parent: Window, x: i16, y: i16) {
        self.create(id, parent, x, y, 0xff0000);
        composite::redirect_window(&self.conn, id, Redirect::MANUAL)
            .unwrap()
            .check()
            .unwrap();
        self.conn.map_window(id).unwrap().check().unwrap();
        let gc = self.conn.generate_id().unwrap();
        self.conn
            .create_gc(gc, id, &CreateGCAux::new().foreground(0x00ff00))
            .unwrap()
            .check()
            .unwrap();
        self.conn
            .poly_fill_rectangle(
                id,
                gc,
                &[Rectangle {
                    x: (WIDTH / 2) as i16,
                    y: 0,
                    width: WIDTH / 2,
                    height: HEIGHT,
                }],
            )
            .unwrap()
            .check()
            .unwrap();
        self.conn.free_gc(gc).unwrap().check().unwrap();
        self.conn.sync().unwrap();
    }

    fn paint_blue(&self, window: Window, width: u16) {
        self.paint_color(window, width, 0x0000ff);
    }

    fn paint_color(&self, window: Window, width: u16, color: u32) {
        let gc = self.conn.generate_id().unwrap();
        self.conn
            .create_gc(gc, window, &CreateGCAux::new().foreground(color))
            .unwrap()
            .check()
            .unwrap();
        self.conn
            .poly_fill_rectangle(
                window,
                gc,
                &[Rectangle {
                    x: 0,
                    y: 0,
                    width,
                    height: HEIGHT,
                }],
            )
            .unwrap()
            .check()
            .unwrap();
        self.conn.free_gc(gc).unwrap().check().unwrap();
        self.conn.sync().unwrap();
    }

    fn capture(&self, window: Window) -> WindowCapture {
        WindowCapture::connect(Some(&self.display), window, 0).unwrap()
    }

    fn root_rgb(&self) -> RgbImage {
        let screen = &self.conn.setup().roots[self.screen_num];
        let visual = screen
            .allowed_depths
            .iter()
            .flat_map(|d| &d.visuals)
            .find(|v| v.visual_id == screen.root_visual)
            .unwrap();
        let reply = self
            .conn
            .get_image(
                ImageFormat::Z_PIXMAP,
                self.root,
                20,
                20,
                WIDTH,
                HEIGHT,
                u32::MAX,
            )
            .unwrap()
            .reply()
            .unwrap();
        let image = XImage::get_from_reply(self.conn.setup(), WIDTH, HEIGHT, reply).unwrap();
        decode_ximage_reusing(
            &image,
            WIDTH,
            HEIGHT,
            PixelLayout::from_visual_type(*visual).unwrap(),
            false,
            &mut CaptureImageRecycler::default(),
        )
        .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Finish resource destruction before releasing the test mutex rather
        // than relying on the server noticing a disconnected socket in time.
        // Only IDs created by this fixture are touched.
        for &window in self.root_windows.borrow().iter().rev() {
            if let Ok(cookie) = self.conn.destroy_window(window) {
                let _ = cookie.check();
            }
        }
        let _ = self.conn.sync();
    }
}

fn frame(capture: &mut WindowCapture) -> RgbImage {
    match capture.frame().expect("capture authorized instance") {
        CaptureOutcome::Frame(frame) => frame.image,
        CaptureOutcome::NoChange | CaptureOutcome::RetainedProofBlocked => {
            panic!("expected a fresh CPU window readback")
        }
    }
}

fn assert_pattern(image: &RgbImage) {
    assert_eq!(image.dimensions(), (WIDTH.into(), HEIGHT.into()));
    for (x, _, pixel) in image.enumerate_pixels() {
        assert_eq!(
            pixel.0,
            if x < u32::from(WIDTH / 2) {
                [255, 0, 0]
            } else {
                [0, 255, 0]
            }
        );
    }
}

#[test]
#[ignore = "requires explicitly configured, dedicated Xvfb via JWM_REMOTE_TEST_DISPLAY"]
fn occlusion_never_substitutes_root_or_occluder_pixels() {
    let fixture = Fixture::new();
    let target = fixture.patterned_target(fixture.root, 20, 20);
    let mut capture = fixture.capture(target);
    assert_pattern(&frame(&mut capture));
    let _cover = fixture.plain_window(fixture.root, 20, 20, 0x0000ff);
    // Prove the real screen rectangle now contains the blue occluder.
    assert!(fixture.root_rgb().pixels().all(|p| p.0 == [0, 0, 255]));
    // The target's named pixmap must still contain only its red/green pattern.
    assert_pattern(&frame(&mut capture));
}

#[test]
#[ignore = "requires explicitly configured, dedicated Xvfb via JWM_REMOTE_TEST_DISPLAY"]
fn destroy_and_exact_xid_reuse_before_first_frame_cannot_inherit_grant() {
    let fixture = Fixture::new();
    let target = fixture.patterned_target(fixture.root, 20, 20);
    let mut capture = fixture.capture(target);
    fixture
        .conn
        .destroy_window(target)
        .unwrap()
        .check()
        .unwrap();
    // Explicitly reuse the same legal client-owned resource ID, not merely a
    // same-title replacement. Both operations precede the first frame call.
    fixture.create_patterned_target(target, fixture.root, 20, 20);
    assert!(
        capture.frame().is_err(),
        "reused XID must not inherit authorization"
    );
    assert!(capture.frame().is_err(), "revocation must stay terminal");
}

#[test]
#[ignore = "requires explicitly configured, dedicated Xvfb via JWM_REMOTE_TEST_DISPLAY"]
fn ancestor_unmap_then_remap_between_frames_revokes_grant() {
    let fixture = Fixture::new();
    let ancestor = fixture.plain_window(fixture.root, 20, 20, 0);
    let target = fixture.patterned_target(ancestor, 0, 0);
    let mut capture = fixture.capture(target);
    assert_pattern(&frame(&mut capture));
    fixture
        .conn
        .unmap_window(ancestor)
        .unwrap()
        .check()
        .unwrap();
    fixture.conn.map_window(ancestor).unwrap().check().unwrap();
    fixture.conn.sync().unwrap();
    assert!(
        capture.frame().is_err(),
        "querying current viewability alone misses this revocation"
    );
}

#[test]
#[ignore = "requires explicitly configured, dedicated Xvfb via JWM_REMOTE_TEST_DISPLAY"]
fn target_resize_then_restore_rebinds_with_new_epoch() {
    let fixture = Fixture::new();
    let target = fixture.patterned_target(fixture.root, 20, 20);
    let mut capture = fixture.capture(target);
    assert_pattern(&frame(&mut capture));
    let previous_epoch = capture.geometry_epoch();
    for width in [u32::from(WIDTH) + 8, u32::from(WIDTH)] {
        fixture
            .conn
            .configure_window(target, &ConfigureWindowAux::new().width(width))
            .unwrap()
            .check()
            .unwrap();
    }
    fixture.paint_blue(target, WIDTH);
    let resized = frame(&mut capture);
    assert_eq!(resized.dimensions(), (WIDTH.into(), HEIGHT.into()));
    assert!(
        resized.pixels().all(|p| p.0 == [0, 0, 255]),
        "old backing store was reused"
    );
    assert!(capture.geometry_epoch() > previous_epoch);
    let stable_epoch = capture.geometry_epoch();
    let _ = frame(&mut capture);
    assert_eq!(capture.geometry_epoch(), stable_epoch);
}

#[test]
#[ignore = "requires explicitly configured, dedicated Xvfb via JWM_REMOTE_TEST_DISPLAY"]
fn local_resize_delivers_new_source_geometry_without_expanding_permissions() {
    let fixture = Fixture::new();
    let target = fixture.patterned_target(fixture.root, 20, 20);
    let mut capture = fixture.capture(target);
    let _ = frame(&mut capture);
    let previous_epoch = capture.geometry_epoch();
    fixture
        .conn
        .configure_window(
            target,
            &ConfigureWindowAux::new().width(u32::from(WIDTH) + 8),
        )
        .unwrap()
        .check()
        .unwrap();
    fixture.paint_blue(target, WIDTH + 8);
    let CaptureOutcome::Frame(resized) = capture.frame().unwrap() else {
        panic!("expected resized frame")
    };
    assert_eq!(
        (resized.source_width, resized.source_height),
        (WIDTH + 8, HEIGHT)
    );
    assert_eq!(
        resized.image.dimensions(),
        (u32::from(WIDTH) + 8, HEIGHT.into())
    );
    assert!(resized.image.pixels().all(|p| p.0 == [0, 0, 255]));
    assert!(resized.geometry_epoch > previous_epoch);
    fixture.conn.unmap_window(target).unwrap().check().unwrap();
    fixture.conn.map_window(target).unwrap().check().unwrap();
    fixture.conn.sync().unwrap();
    assert!(
        capture.frame().is_err(),
        "resize support must not weaken unmap revocation"
    );
}

#[test]
#[ignore = "requires explicitly configured, dedicated Xvfb via JWM_REMOTE_TEST_DISPLAY"]
fn one_hundred_local_resizes_keep_fresh_geometry_and_pixels_then_revoke_on_reuse() {
    let fixture = Fixture::new();
    let target = fixture.patterned_target(fixture.root, 20, 20);
    let mut capture = fixture.capture(target);
    let _ = frame(&mut capture);
    let mut previous_epoch = capture.geometry_epoch();
    for step in 1..=100_u16 {
        let width = WIDTH + step % 8;
        fixture
            .conn
            .configure_window(target, &ConfigureWindowAux::new().width(u32::from(width)))
            .unwrap()
            .check()
            .unwrap();
        let color = if step % 2 == 0 { 0x0000ff } else { 0xff00ff };
        let expected = if step % 2 == 0 {
            [0, 0, 255]
        } else {
            [255, 0, 255]
        };
        fixture.paint_color(target, width, color);
        // Stable, synchronized fixture geometry must settle within a bounded
        // number of readback retries; do not hide an endless NoChange loop.
        let mut fresh = None;
        for _ in 0..4 {
            if let CaptureOutcome::Frame(frame) = capture.frame().unwrap() {
                fresh = Some(frame);
                break;
            }
        }
        let fresh = fresh.expect("stable resized window failed to produce a fresh frame");
        assert_eq!((fresh.source_width, fresh.source_height), (width, HEIGHT));
        assert_eq!(fresh.image.dimensions(), (width.into(), HEIGHT.into()));
        assert!(
            fresh.geometry_epoch > previous_epoch,
            "resize {step} reused geometry epoch"
        );
        assert!(
            fresh.image.pixels().all(|p| p.0 == expected),
            "resize {step} reused stale pixels"
        );
        previous_epoch = fresh.geometry_epoch;
    }
    fixture
        .conn
        .destroy_window(target)
        .unwrap()
        .check()
        .unwrap();
    fixture.create_patterned_target(target, fixture.root, 20, 20);
    assert!(
        capture.frame().is_err(),
        "resize must not enable XID reuse authorization"
    );
}

#[test]
#[ignore = "requires explicitly configured, dedicated Xvfb via JWM_REMOTE_TEST_DISPLAY"]
fn resize_restore_after_naming_discards_candidate_before_readback() {
    let fixture = Fixture::new();
    let target = fixture.patterned_target(fixture.root, 20, 20);
    let mut capture = fixture.capture(target);
    let _ = frame(&mut capture);
    let previous_epoch = capture.geometry_epoch();
    fixture
        .conn
        .configure_window(
            target,
            &ConfigureWindowAux::new().width(u32::from(WIDTH) + 8),
        )
        .unwrap()
        .check()
        .unwrap();
    fixture.paint_blue(target, WIDTH + 8);
    let accepted = capture
        .rebind_with_test_hook(|| {
            for width in [u32::from(WIDTH) + 16, u32::from(WIDTH) + 8] {
                fixture
                    .conn
                    .configure_window(target, &ConfigureWindowAux::new().width(width))
                    .unwrap()
                    .check()
                    .unwrap();
            }
            fixture.paint_color(target, WIDTH + 8, 0xff00ff);
        })
        .unwrap();
    assert!(
        !accepted,
        "candidate named before resize/restore must be discarded"
    );
    assert_eq!(
        capture.geometry_epoch(),
        previous_epoch,
        "discarded candidate must not become current"
    );
    let fresh = frame(&mut capture);
    assert_eq!(fresh.dimensions(), (u32::from(WIDTH) + 8, HEIGHT.into()));
    assert!(fresh.pixels().all(|p| p.0 == [255, 0, 255]));
    assert!(capture.geometry_epoch() > previous_epoch);
}

#[test]
#[ignore = "requires explicitly configured, dedicated Xvfb via JWM_REMOTE_TEST_DISPLAY"]
fn destroy_exact_xid_reuse_after_naming_permanently_revokes_before_readback() {
    let fixture = Fixture::new();
    let target = fixture.patterned_target(fixture.root, 20, 20);
    let mut capture = fixture.capture(target);
    let _ = frame(&mut capture);
    fixture
        .conn
        .configure_window(
            target,
            &ConfigureWindowAux::new().width(u32::from(WIDTH) + 8),
        )
        .unwrap()
        .check()
        .unwrap();
    fixture.paint_blue(target, WIDTH + 8);
    let result = capture.rebind_with_test_hook(|| {
        fixture
            .conn
            .destroy_window(target)
            .unwrap()
            .check()
            .unwrap();
        fixture.create_patterned_target(target, fixture.root, 20, 20);
    });
    assert!(
        result.is_err(),
        "destroy/reuse after naming must revoke, not merely retry"
    );
    assert!(
        capture.frame().is_err(),
        "replacement must never become the capture target"
    );
}

#[test]
#[ignore = "requires explicitly configured, dedicated Xvfb via JWM_REMOTE_TEST_DISPLAY"]
fn depth32_visual_is_rejected_when_server_supports_it() {
    let fixture = Fixture::new();
    let screen = &fixture.conn.setup().roots[fixture.screen_num];
    let Some(visual) = screen
        .allowed_depths
        .iter()
        .filter(|d| d.depth == 32)
        .flat_map(|d| &d.visuals)
        .find(|v| v.class == VisualClass::TRUE_COLOR)
    else {
        eprintln!("SKIP depth32 subcase: dedicated X server has no 32-bit TrueColor visual");
        return;
    };
    let colormap = fixture.conn.generate_id().unwrap();
    fixture
        .conn
        .create_colormap(
            ColormapAlloc::NONE,
            colormap,
            fixture.root,
            visual.visual_id,
        )
        .unwrap()
        .check()
        .unwrap();
    let target = fixture.conn.generate_id().unwrap();
    fixture
        .conn
        .create_window(
            32,
            target,
            fixture.root,
            20,
            20,
            WIDTH,
            HEIGHT,
            0,
            WindowClass::INPUT_OUTPUT,
            visual.visual_id,
            &CreateWindowAux::new()
                .colormap(colormap)
                .border_pixel(0)
                .background_pixel(0),
        )
        .unwrap()
        .check()
        .unwrap();
    fixture.root_windows.borrow_mut().push(target);
    composite::redirect_window(&fixture.conn, target, Redirect::MANUAL)
        .unwrap()
        .check()
        .unwrap();
    fixture.conn.map_window(target).unwrap().check().unwrap();
    fixture.conn.sync().unwrap();
    match WindowCapture::connect(Some(&fixture.display), target, 0) {
        Ok(_) => panic!("unsupported alpha-capable visual was accepted"),
        Err(error) => assert!(error.to_string().contains("24-bit TrueColor"), "{error}"),
    }
}
