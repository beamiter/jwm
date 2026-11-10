//! Runner-owned authenticated TCP, production codec and actual receive-loop reset.
//! No capture/display or presentation claim. Never run against a user desktop.
use super::*;
use crate::remote::messages::StreamCapabilities;
use crate::remote::protocol::server_handshake;
use crate::remote::source_session::{SourceId, SourceVersion};
use crate::remote::source_wire::{
    FramePurpose, SourceEntry, encode_frame_payload, encode_source_changed,
};
use crate::remote::tile::{TileEncodeRequest, TileEncoder};
use crate::remote::x11_capture::CapturedFrame;
use image::{Rgb, RgbImage};
use std::net::TcpListener;

const KEY: [u8; 32] = [0x39; 32];
#[derive(Clone, Copy, Debug)]
enum Case {
    EnabledSourceReset,
    Disabled,
    OldBaseAfterReset,
    ReversedBand,
    BandPastCanvas,
}
fn frame(offset: u32) -> CapturedFrame {
    CapturedFrame {
        geometry_epoch: 1,
        source_width: 96,
        source_height: 80,
        image: RgbImage::from_fn(96, 80, |x, y| {
            let y = y + offset;
            Rgb([
                (x * 17 + y * 29) as u8,
                (y * 53 + x * 3) as u8,
                (x ^ (y * 11)) as u8,
            ])
        }),
    }
}
fn pair(first_sequence: u64, band: Option<i16>) -> Vec<Vec<u8>> {
    let mut encoder = TileEncoder::new();
    encoder.set_scroll_copy_enabled(true);
    let mut result = Vec::new();
    for (index, offset) in [0, 7].into_iter().enumerate() {
        let frame = if let Some(dy) = band {
            band_frame(if index == 0 { 0 } else { dy })
        } else {
            frame(offset)
        };
        assert!(encoder.plan(&frame, TileEncodeRequest::Delta).unwrap().emit);
        let mut payload = Vec::new();
        encoder
            .encode_into(&mut payload, first_sequence + index as u64, &frame, 85)
            .unwrap();
        encoder.commit(&frame);
        assert_eq!(
            payload[17] & 2 != 0,
            index == 1,
            "must exercise a real production copy record"
        );
        if index == 1 {
            assert_eq!(
                u64::from_be_bytes(payload[32..40].try_into().unwrap()),
                first_sequence
            );
            if let Some(dy) = band {
                assert_eq!(i16::from_be_bytes(payload[40..42].try_into().unwrap()), dy);
                assert_eq!(u16::from_be_bytes(payload[42..44].try_into().unwrap()), 32);
                assert_eq!(u16::from_be_bytes(payload[44..46].try_into().unwrap()), 128);
            }
        }
        result.push(payload);
    }
    if band.is_some() {
        let mut decoder = TileDecoder::new();
        decoder.set_scroll_copy_enabled(true);
        let pool = new_decode_buffer_pool();
        let before = decoder.decode_into(&result[0], Arc::clone(&pool)).unwrap();
        let after = decoder.decode_into(&result[1], pool).unwrap();
        for y in (0..32).chain(128..160) {
            for x in 0..96 {
                assert_eq!(
                    before.image().get_pixel(x, y),
                    after.image().get_pixel(x, y),
                    "fixed UI moved at {x},{y}"
                );
            }
        }
    }
    result
}

fn band_frame(dy: i16) -> CapturedFrame {
    CapturedFrame {
        geometry_epoch: 1,
        source_width: 96,
        source_height: 160,
        image: RgbImage::from_fn(96, 160, |x, y| {
            let row = if (32..128).contains(&y) {
                (y as i32 + i32::from(dy)) as u32
            } else {
                y
            };
            let mut pixel = [
                (x * 17 + row * 29) as u8,
                (row * 53 + x * 3) as u8,
                (x ^ (row * 11)) as u8,
            ];
            // Newly exposed content is not copied from the fixed toolbar/footer.
            // Otherwise the exact matching run legitimately extends beyond the
            // intended band and the encoder infers a wider copy interval.
            if (32..128).contains(&y) && !(32..128).contains(&row) {
                for channel in &mut pixel {
                    *channel ^= 0xa5;
                }
            }
            Rgb(pixel)
        }),
    }
}

#[test]
fn fixed_band_codec_pairs_preserve_toolbar_footer_without_transport() {
    for dy in [-7, 7] {
        let records = pair(1, Some(dy));
        assert_eq!(records.len(), 2);
    }
}

/// Pure fixture proof: no socket, decoder or encoder assumptions are involved.
#[test]
fn fixed_band_fixture_has_exact_overlap_and_genuinely_new_exposed_rows() {
    let initial = band_frame(0);
    for dy in [-7_i16, 7] {
        let next = band_frame(dy);
        for y in 0..160 {
            for x in 0..96 {
                if !(32..128).contains(&y) {
                    assert_eq!(next.image.get_pixel(x, y), initial.image.get_pixel(x, y));
                } else {
                    let source_y = (y as i32 + i32::from(dy)) as u32;
                    if (32..128).contains(&source_y) {
                        assert_eq!(
                            next.image.get_pixel(x, y),
                            initial.image.get_pixel(x, source_y)
                        );
                    } else {
                        assert_ne!(
                            next.image.get_pixel(x, y),
                            initial.image.get_pixel(x, source_y)
                        );
                    }
                }
            }
        }
        // Outside rows adjacent to the band also cannot extend the matching run.
        for y in [31_u32, 128] {
            let source_y = (y as i32 + i32::from(dy)) as u32;
            assert!(
                (0..96).any(|x| next.image.get_pixel(x, y) != initial.image.get_pixel(x, source_y))
            );
        }
    }
}

#[test]
fn authenticated_tcp_scroll_copy_permission_and_source_reset() {
    for band in [None, Some(7), Some(-7)] {
        for case in [
            Case::EnabledSourceReset,
            Case::Disabled,
            Case::OldBaseAfterReset,
            Case::ReversedBand,
            Case::BandPastCanvas,
        ] {
            let enabled = !matches!(case, Case::Disabled);
            let malformed = matches!(case, Case::ReversedBand | Case::BandPastCanvas);
            if malformed && band.is_none() {
                continue;
            }
            let mut first = pair(1, band);
            if matches!(case, Case::ReversedBand) {
                first[1][42..44].copy_from_slice(&128_u16.to_be_bytes());
            }
            if matches!(case, Case::BandPastCanvas) {
                first[1][44..46].copy_from_slice(&161_u16.to_be_bytes());
            }
            let mut records = Vec::new();
            let mut expected = Vec::new();
            let pool = new_decode_buffer_pool();
            let mut reference = TileDecoder::new();
            reference.set_scroll_copy_enabled(true);
            for payload in &first {
                records.push((
                    MessageKind::Frame,
                    encode_frame_payload(1, FramePurpose::Video, payload).unwrap(),
                ));
                if (enabled && !malformed) || expected.is_empty() {
                    let decoded = reference.decode_into(payload, Arc::clone(&pool)).unwrap();
                    expected.push((1, decoded.sequence(), decoded.image().as_raw().clone()));
                }
            }
            if enabled && !malformed {
                records.push((
                    MessageKind::SourceChanged,
                    encode_source_changed(SourceVersion {
                        epoch: 2,
                        source: SourceId(2),
                    })
                    .unwrap()
                    .to_vec(),
                ));
                if matches!(case, Case::OldBaseAfterReset) {
                    // Establish valid geometry and a new-source base, then replay
                    // an old-source copy with a globally valid next sequence. The
                    // rejection must be the base mismatch, not a missing keyframe.
                    let keyframe = pair(3, band).remove(0);
                    reference = TileDecoder::new();
                    reference.set_scroll_copy_enabled(true);
                    let decoded = reference.decode_into(&keyframe, Arc::clone(&pool)).unwrap();
                    expected.push((2, decoded.sequence(), decoded.image().as_raw().clone()));
                    records.push((
                        MessageKind::Frame,
                        encode_frame_payload(2, FramePurpose::Video, &keyframe).unwrap(),
                    ));
                    let mut old = first[1].clone();
                    old[..8].copy_from_slice(&4u64.to_be_bytes());
                    records.push((
                        MessageKind::Frame,
                        encode_frame_payload(2, FramePurpose::Video, &old).unwrap(),
                    ));
                } else {
                    reference = TileDecoder::new();
                    reference.set_scroll_copy_enabled(true);
                    for payload in pair(3, band) {
                        let decoded = reference.decode_into(&payload, Arc::clone(&pool)).unwrap();
                        expected.push((2, decoded.sequence(), decoded.image().as_raw().clone()));
                        records.push((
                            MessageKind::Frame,
                            encode_frame_payload(2, FramePurpose::Video, &payload).unwrap(),
                        ));
                    }
                }
            }
            records.push((MessageKind::Close, b"owned codec fixture complete".to_vec()));
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let server = std::thread::spawn(move || {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                socket
                    .set_write_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let (receive, send) = server_handshake(&mut socket, &KEY, [0x55; 32])
                    .unwrap()
                    .into_server();
                let mut reader = SessionReader::new(socket.try_clone().unwrap(), receive);
                let mut writer = SessionWriter::new(socket, send);
                let (kind, payload) = reader.read_message().unwrap();
                assert_eq!(kind, MessageKind::Hello);
                let hello = ClientHello::decode(&payload).unwrap();
                assert!(hello.stream.scroll_copy);
                let negotiated = hello.stream.negotiate_scroll_copy(enabled);
                assert_eq!(negotiated, enabled);
                writer
                    .write_message(
                        MessageKind::HelloAck,
                        &ServerHello {
                            stream: StreamCapabilities {
                                scroll_copy: negotiated,
                                ..Default::default()
                            },
                            pointer_enabled: false,
                            keyboard_enabled: false,
                            clipboard_enabled: false,
                            clipboard_files_enabled: false,
                        }
                        .encode(),
                    )
                    .unwrap();
                let catalog = SourceCatalog {
                    current: SourceVersion {
                        epoch: 1,
                        source: SourceId(1),
                    },
                    entries: vec![
                        SourceEntry {
                            id: SourceId(1),
                            label: "owned-first".into(),
                        },
                        SourceEntry {
                            id: SourceId(2),
                            label: "owned-second".into(),
                        },
                    ],
                };
                writer
                    .write_message(MessageKind::SourceCatalog, &catalog.encode().unwrap())
                    .unwrap();
                for (kind, payload) in records {
                    writer.write_message(kind, &payload).unwrap();
                }
                writer.flush().unwrap();
            });
            let mut socket = TcpStream::connect_timeout(&address, Duration::from_secs(5)).unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            socket
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let (receive, send) = client_handshake(&mut socket, &KEY, [0x66; 32])
                .unwrap()
                .into_client();
            let mut reader = SessionReader::new(socket.try_clone().unwrap(), receive);
            let mut writer = SessionWriter::new(socket, send);
            writer
                .write_message(
                    MessageKind::Hello,
                    &ClientHello {
                        stream: StreamCapabilities {
                            scroll_copy: true,
                            ..Default::default()
                        },
                        initial_viewport: None,
                        request_input: false,
                        request_clipboard: false,
                        request_clipboard_files: false,
                        keymap: KeymapTable::empty(),
                    }
                    .encode(),
                )
                .unwrap();
            writer.flush().unwrap();
            let hello = read_server_hello(&mut reader).unwrap();
            assert_eq!(hello.stream.scroll_copy, enabled);
            assert!(!hello.pointer_enabled && !hello.keyboard_enabled && !hello.clipboard_enabled);
            let mut initial = ReceiveState::new();
            initial.scroll_copy = hello.stream.scroll_copy;
            initial.frame_limits = hello.stream.frame_limits;
            let state = Arc::new(initial);
            let (kind, catalog) = reader.read_message().unwrap();
            assert_eq!(kind, MessageKind::SourceCatalog);
            receive_catalog(&state, &catalog).unwrap();
            let decoder = state.new_tile_decoder().unwrap();
            let (_wake_receiver, wake_sender) = wake_pair().unwrap();
            let mut observed = Vec::new();
            receive_frame_records(
                |payload, _remaining| {
                    // Observe only after the production receive loop handled the prior
                    // record. This does not feed authority or forge presentation ACKs.
                    if let Some(queued) = state.latest.lock().unwrap().as_ref() {
                        let key = (queued.frame.source_epoch(), queued.frame.sequence());
                        if observed
                            .last()
                            .is_none_or(|(epoch, seq, _)| (*epoch, *seq) != key)
                        {
                            observed.push((key.0, key.1, queued.frame.image().as_raw().clone()));
                        }
                    }
                    reader
                        .read_message_into(payload)
                        .map_err(receiver_read_error)
                },
                Arc::clone(&state),
                0,
                Vec::new(),
                wake_sender,
                decoder,
                None,
            );
            server.join().unwrap();
            assert_eq!(observed, expected, "case={case:?}");
            let error = state
                .error
                .lock()
                .unwrap()
                .take()
                .expect("terminal outcome");
            if matches!(case, Case::EnabledSourceReset) {
                assert!(error.is::<PeerClosed>(), "{error}");
                assert_eq!(state.source_epoch.load(Ordering::Acquire), 2);
            } else {
                let reason = if malformed {
                    "scroll displacement out of range"
                } else {
                    "scroll copy capability or base mismatch"
                };
                assert!(error.to_string().contains(reason), "case={case:?}: {error}");
                if malformed {
                    let latest = state.latest.lock().unwrap();
                    let frame = &latest.as_ref().unwrap().frame;
                    assert_eq!(frame.sequence(), 1);
                    assert_eq!(frame.source_epoch(), 1);
                    assert_eq!(frame.image().as_raw(), &expected[0].2);
                }
                if matches!(case, Case::OldBaseAfterReset) {
                    assert_eq!(state.source_epoch.load(Ordering::Acquire), 2);
                    assert_eq!(
                        state
                            .latest
                            .lock()
                            .unwrap()
                            .as_ref()
                            .unwrap()
                            .frame
                            .sequence(),
                        3
                    );
                    assert!(
                        state.scroll_copy,
                        "reset must preserve negotiated capability while dropping reference"
                    );
                }
            }
        }
    }
}

/// Actual failed encrypted TCP write plus explicit codec discard. This tests
/// the codec/transport contract, not the private host send-loop commit branch.
#[test]
fn authenticated_tcp_failed_band_write_discard_preserves_reference_and_base() {
    for dy in [-7, 7] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            socket
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let (receive, _) = server_handshake(&mut socket, &KEY, [0x71; 32])
                .unwrap()
                .into_server();
            let mut reader = SessionReader::new(socket, receive);
            // Local write shutdown deterministically prevents sending any record.
            assert!(
                matches!(reader.read_message(), Err(crate::remote::protocol::ProtocolError::Io(error)) if error.kind() == std::io::ErrorKind::UnexpectedEof)
            );
        });
        let mut socket = TcpStream::connect_timeout(&address, Duration::from_secs(5)).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let (_, send) = client_handshake(&mut socket, &KEY, [0x72; 32])
            .unwrap()
            .into_client();
        socket.shutdown(std::net::Shutdown::Write).unwrap();
        let mut writer = SessionWriter::new(socket, send);
        let mut encoder = TileEncoder::new();
        encoder.set_scroll_copy_enabled(true);
        let initial = band_frame(0);
        encoder.plan(&initial, TileEncodeRequest::Delta).unwrap();
        let mut initial_bytes = Vec::new();
        encoder
            .encode_into(&mut initial_bytes, 1, &initial, 85)
            .unwrap();
        encoder.commit(&initial);
        let next = band_frame(dy);
        encoder.plan(&next, TileEncodeRequest::Delta).unwrap();
        let mut failed = Vec::new();
        encoder.encode_into(&mut failed, 2, &next, 85).unwrap();
        assert_ne!(failed[17] & 2, 0);
        assert!(
            writer
                .write_message(
                    MessageKind::Frame,
                    &encode_frame_payload(1, FramePurpose::Video, &failed).unwrap()
                )
                .is_err()
        );
        encoder.discard();
        assert_eq!(encoder.reference(), Some(&initial.image));
        encoder.plan(&next, TileEncodeRequest::Delta).unwrap();
        let mut retry = Vec::new();
        encoder.encode_into(&mut retry, 3, &next, 85).unwrap();
        assert_ne!(retry[17] & 2, 0);
        assert_eq!(u64::from_be_bytes(retry[32..40].try_into().unwrap()), 1);
        assert_eq!(i16::from_be_bytes(retry[40..42].try_into().unwrap()), dy);
        let mut decoder = TileDecoder::new();
        decoder.set_scroll_copy_enabled(true);
        decoder
            .decode_into(&initial_bytes, new_decode_buffer_pool())
            .unwrap();
        assert_eq!(
            decoder
                .decode_into(&retry, new_decode_buffer_pool())
                .unwrap()
                .sequence(),
            3
        );
        encoder.discard();
        server.join().unwrap();
    }
}
