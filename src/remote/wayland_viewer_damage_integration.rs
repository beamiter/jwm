//! Actual two-slot SHM and renderer checks for cumulative tile damage. The
//! owned server controls feedback; this is not physical-display timing.
use super::*;
use crate::remote::frame::TileSnapshot;

fn assert_shm_image(buffer: &WlBuffer, expected: &RgbImage) {
    smithay::wayland::shm::with_buffer_contents(buffer, |ptr, len, data| {
        assert_eq!((data.width, data.height, data.stride), (800, 600, 3200));
        let offset = usize::try_from(data.offset).unwrap();
        let bytes = 800 * 600 * 4;
        assert!(offset.checked_add(bytes).is_some_and(|end| end <= len));
        // SAFETY: this live SHM mapping contains the entire checked range.
        let pixels = unsafe { std::slice::from_raw_parts(ptr.add(offset), bytes) };
        for (actual, expected) in pixels.chunks_exact(4).zip(expected.pixels()) {
            let word = u32::from_ne_bytes(actual.try_into().unwrap());
            assert_eq!(
                word,
                (u32::from(expected[0]) << 16)
                    | (u32::from(expected[1]) << 8)
                    | u32::from(expected[2]),
                "complete SHM canvas must include skipped-frame and old-slot damage"
            );
        }
    })
    .unwrap();
}

fn assert_gl_image(
    server: &mut Server,
    gl: &mut HeadlessSmithayGles,
    capture: &mut Capture,
    expected: &RgbImage,
) {
    let (buffer, mut file) = shm_buffer_file(server, &mut capture.client, [0x5a; 4]);
    let frame = capture.client.new_id();
    capture.client.request(capture.session, 0, &[frame]);
    capture.client.request(frame, 1, &[buffer]);
    capture.client.request(frame, 3, &[]);
    server.roundtrip();
    let queue = server.state.image_capture_pending.as_ref().unwrap().clone();
    assert_eq!(queue.lock().unwrap().len(), 1);
    fulfill_test_toplevel_capture(&mut gl.renderer, &mut None, &server.state, &queue);
    server.roundtrip();
    let events = capture.client.events();
    assert!(events.iter().all(|e| !(e.sender == 1 && e.opcode == 0)));
    assert!(events.iter().any(|e| e.sender == frame && e.opcode == 3));
    file.seek(SeekFrom::Start(0)).unwrap();
    let mut pixels = Vec::new();
    file.read_to_end(&mut pixels).unwrap();
    assert_eq!(pixels.len(), 800 * 600 * 4);
    for (index, (actual, expected)) in pixels.chunks_exact(4).zip(expected.pixels()).enumerate() {
        assert!(
            actual[0].abs_diff(expected[2]) <= 3
                && actual[1].abs_diff(expected[1]) <= 3
                && actual[2].abs_diff(expected[0]) <= 3
                && actual[3] == 255,
            "actual GL canvas pixel {index} must match cumulative tile state"
        );
    }
    capture.client.request(frame, 0, &[]);
    server.roundtrip();
}

#[test]
fn actual_wayland_viewer_accumulates_skipped_tile_damage_across_held_slots() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let (mut server, peer) = server_and_peer(false);
    assert!(matches!(peer.recv(&mut server), Reply::Connected));
    let surface = viewer_surface(&server);
    let mut canvas = RgbImage::from_pixel(800, 600, Rgb([31, 63, 95]));
    let tile = 64;
    let columns = 800usize.div_ceil(tile);
    let count = columns * 600usize.div_ceil(tile);
    let mut tiles = TileSnapshot::update(&canvas, None, &vec![255; count.div_ceil(8)], tile);
    let mut capture = None;
    let mut previous: Option<(smithay::backend::renderer::utils::Buffer, RgbImage)> = None;
    let mut identities = Vec::new();
    for sequence in 1u64..=7 {
        let mut bitmap = vec![0u8; count.div_ceil(8)];
        if sequence > 1 && sequence != 5 {
            // Frame 2 is never submitted. Frame 3 changes a different tile:
            // the viewer must preserve both, then carry both into the older
            // SHM slot when it is released and reused for frame 4.
            let (x, y) = match sequence {
                2 => (1, 1),
                3 => (799, 599),
                4 => (390, 290),
                6 => (700, 100),
                _ => (1, 599),
            };
            canvas.put_pixel(x, y, Rgb([sequence as u8 * 27, 149, 211]));
            let index = y as usize / tile * columns + x as usize / tile;
            bitmap[index / 8] |= 1 << (index % 8);
            tiles = TileSnapshot::update(&canvas, Some(&tiles), &bitmap, tile);
        }
        if sequence == 2 {
            continue;
        }
        let frame = RecyclableDecodedFrame::from_tiles(
            sequence,
            800,
            600,
            tiles.clone(),
            new_decode_buffer_pool(),
        );
        peer.command
            .send(Command::DrawFrame(Box::new(frame)))
            .unwrap();
        assert!(matches!(peer.recv(&mut server), Reply::Drawn(true)));
        let feedback = wait_feedback(&mut server, &surface);
        let held = with_renderer_surface_state(&surface, |s| s.buffer().cloned())
            .flatten()
            .unwrap();
        if !identities.contains(&held.id()) {
            identities.push(held.id());
        }
        assert!(identities.len() <= 2);
        assert_shm_image(&held, &canvas);
        if let Some((old, expected)) = previous.as_ref() {
            assert_ne!(old.id(), held.id(), "busy SHM slot must stay untouched");
            assert_shm_image(old, expected);
        }
        let capture = capture.get_or_insert_with(|| Capture::new(&mut server));
        assert_gl_image(&mut server, &mut gl, capture, &canvas);
        drop(previous.take());
        frame_callback_only(&surface);
        server.roundtrip();
        let waiting = peer.poll(&mut server);
        assert!(waiting.presented.is_empty());
        assert!(!waiting.can_submit);
        present(&mut server, feedback);
        let done = peer.poll(&mut server);
        assert_eq!(done.presented, [sequence]);
        assert!(done.can_submit);
        assert!(peer.poll(&mut server).presented.is_empty());
        previous = Some((held, canvas.clone()));
    }
    assert_eq!(identities.len(), 2);
    peer.stop(&mut server);
    drop(previous);
}
