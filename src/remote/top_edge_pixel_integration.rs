//! CI-only software-path isolation. Passing is not NVIDIA hardware validation.
use super::*;
use crate::remote::frame::new_decode_buffer_pool;
use crate::remote::tile::{TileDecoder, TileEncodeRequest, TileEncoder};
use crate::remote::x11_capture::{CaptureImageRecycler, CapturedFrame, decode_ximage_reusing};

fn read_pixels(viewer: &Viewer, drawable: u32) -> RgbImage {
    let reply = viewer
        .conn
        .get_image(
            ImageFormat::Z_PIXMAP,
            drawable,
            0,
            0,
            viewer.width,
            viewer.height,
            u32::MAX,
        )
        .unwrap()
        .reply()
        .unwrap();
    let image =
        XImage::get_from_reply(viewer.conn.setup(), viewer.width, viewer.height, reply).unwrap();
    decode_ximage_reusing(
        &image,
        viewer.width,
        viewer.height,
        viewer.pixel_layout,
        false,
        &mut CaptureImageRecycler::default(),
    )
    .unwrap()
}

fn finish(viewer: &mut Viewer, sequence: u64) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        viewer.poll_events().unwrap();
        if viewer
            .take_presented_sequences()
            .unwrap()
            .contains(&sequence)
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "presentation timeout at {sequence}"
        );
        std::thread::yield_now();
    }
}

fn oracle(image: &RgbImage, width: u16, height: u16) -> RgbImage {
    let bounds = letterbox(width, height, image.width() as u16, image.height() as u16).unwrap();
    let scaled = image::imageops::resize(
        image,
        u32::from(bounds.width),
        u32::from(bounds.height),
        image::imageops::FilterType::Triangle,
    );
    let mut result = RgbImage::new(u32::from(width), u32::from(height));
    image::imageops::replace(
        &mut result,
        &scaled,
        i64::from(bounds.x),
        i64::from(bounds.y),
    );
    result
}

#[test]
#[ignore = "requires the explicit CI-owned Xvfb runner; never use ambient DISPLAY"]
fn actual_top_edge_codec_scaled_core_and_shm_pixels() {
    assert_eq!(
        std::env::var("JWM_REMOTE_TEST_VIEWER_OWNED").as_deref(),
        Ok("1")
    );
    let display = std::env::var("JWM_REMOTE_TEST_VIEWER_DISPLAY").unwrap();
    for shm in [false, true] {
        let mut viewer = Viewer::connect_with_window_size(
            Some(&display),
            641,
            359,
            Some(crate::remote::window_size::WindowSize {
                width: 1001,
                height: 601,
            }),
            ScalingPolicy::Cpu,
            false,
            false,
            None,
            false,
            "F12",
            "top-edge-regression",
        )
        .unwrap();
        if shm {
            assert!(
                matches!(viewer.shm_upload, ShmUpload::Enabled(_)),
                "CI must exercise SHM, not fallback"
            );
        } else {
            viewer.shm_upload = ShmUpload::Disabled;
        }
        viewer.conn.get_input_focus().unwrap().reply().unwrap();
        viewer.poll_events().unwrap();
        let mut encoder = TileEncoder::new();
        let mut decoder = TileDecoder::new();
        let pool = new_decode_buffer_pool();
        for sequence in 1..=40u64 {
            if sequence == 20 {
                viewer
                    .conn
                    .configure_window(
                        viewer.window,
                        &x11rb::protocol::xproto::ConfigureWindowAux::new()
                            .width(997)
                            .height(713),
                    )
                    .unwrap()
                    .check()
                    .unwrap();
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    viewer.poll_events().unwrap();
                    if viewer.viewport() == (997, 713) {
                        break;
                    }
                    assert!(
                        Instant::now() < deadline,
                        "real ConfigureNotify did not arrive"
                    );
                    std::thread::yield_now();
                }
            }
            // Odd dimensions, a geometry/aspect change, and sparse changes confined
            // to the first 1..33 rows. No black source pixels can masquerade as bars.
            let (width, height, epoch) = if sequence < 36 {
                (641, 359, 1)
            } else {
                (513, 377, 2)
            };
            let mut image = RgbImage::from_pixel(width, height, image::Rgb([91, 157, 203]));
            for y in 0..((sequence as u32 % 33) + 1) {
                for x in 0..width {
                    if (x / 7 + y + sequence as u32) % 11 == 0 {
                        image.put_pixel(x, y, image::Rgb([213, 83, 149]));
                    }
                }
            }
            let frame = CapturedFrame {
                image,
                geometry_epoch: epoch,
                source_width: width as u16,
                source_height: height as u16,
            };
            assert!(
                encoder
                    .plan(&frame, TileEncodeRequest::FreshProof)
                    .unwrap()
                    .emit
            );
            let mut payload = Vec::new();
            encoder
                .encode_into(&mut payload, sequence, &frame, 90)
                .unwrap();
            encoder.commit(&frame);
            let decoded = decoder.decode_into(&payload, pool.clone()).unwrap();
            // Independently inspect codec output before any upload or scaling.
            assert!(
                decoded
                    .image()
                    .pixels()
                    .all(|p| p.0.iter().any(|c| *c > 24)),
                "codec introduced a black block: shm={shm} sequence={sequence}"
            );
            let expected = oracle(decoded.image(), viewer.width, viewer.height);
            // Decode every record, deliberately omit presentation of predecessors.
            if sequence % 7 == 0 {
                continue;
            }
            assert!(viewer.draw_recyclable(decoded).unwrap());
            if shm && sequence == 1 {
                // Enabled alone is insufficient: a saturated pool may take the
                // core PutImage fallback. The first full upload must issue a
                // real SHM request and retain its exact completion key before
                // we dispatch any completion events. `reported_active` is set
                // only after shm::put_image successfully returns its cookie.
                match &viewer.shm_upload {
                    ShmUpload::Enabled(upload) => {
                        assert!(upload.reported_active, "no actual SHM upload issued");
                        assert!(
                            upload.slots.iter().any(|slot| slot.completion.is_some()),
                            "actual SHM request must await its server completion"
                        );
                    }
                    ShmUpload::Disabled => panic!("SHM positive control fell back"),
                }
            }
            finish(&mut viewer, sequence);
            assert!(
                viewer.shm_upload.pending_keys().is_empty(),
                "presentation must drain exact SHM completion before reusing slots"
            );
            assert_eq!(
                read_pixels(&viewer, viewer.backing),
                expected,
                "backing pollution: shm={shm} sequence={sequence}"
            );
            assert_eq!(
                read_pixels(&viewer, viewer.window),
                expected,
                "window pollution after backing: shm={shm} sequence={sequence}"
            );
            if shm {
                assert!(
                    matches!(viewer.shm_upload, ShmUpload::Enabled(_)),
                    "SHM unexpectedly fell back"
                );
            }
        }
        // Cursor compositing remains a distinct XRender operation even with CPU
        // scaling. Exercise it away from the top rows, then erase it again.
        let before = read_pixels(&viewer, viewer.window);
        viewer.enable_cursor_plane().unwrap();
        let shape = Arc::new(CursorShape {
            id: 1,
            width: 4,
            height: 4,
            hotspot_x: 0,
            hotspot_y: 0,
            rgba: [255, 0, 0, 255].repeat(16),
        });
        let mut position = CursorPosition {
            sequence: 1,
            geometry_epoch: 2,
            shape_id: 1,
            source_width: 513,
            source_height: 377,
            x: 250,
            y: 200,
            visible: true,
        };
        viewer.set_cursor_update(shape.clone(), position).unwrap();
        let visible = read_pixels(&viewer, viewer.window);
        assert_ne!(
            visible, before,
            "cursor positive control must paint actual pixels"
        );
        for y in 0..100 {
            for x in 0..visible.width() {
                assert_eq!(
                    visible.get_pixel(x, y),
                    before.get_pixel(x, y),
                    "cursor corrupted top rows"
                );
            }
        }
        position.sequence = 2;
        position.visible = false;
        viewer.set_cursor_update(shape, position).unwrap();
        assert_eq!(
            read_pixels(&viewer, viewer.window),
            before,
            "cursor hide left residue"
        );
    }
}
