//! Runner-owned authenticated TCP, production codec and actual receive-loop reset.
//! No capture/display or presentation claim. Never run against a user desktop.
use super::*;
use crate::remote::messages::StreamCapabilities;
use crate::remote::protocol::server_handshake;
use crate::remote::source_session::{SourceId, SourceVersion};
use crate::remote::source_wire::{SourceEntry, encode_source_changed, encode_source_payload};
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
fn pair(first_sequence: u64) -> Vec<Vec<u8>> {
    let mut encoder = TileEncoder::new();
    encoder.set_scroll_copy_enabled(true);
    let mut result = Vec::new();
    for (index, offset) in [0, 7].into_iter().enumerate() {
        let frame = frame(offset);
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
        result.push(payload);
    }
    result
}

#[test]
fn authenticated_tcp_scroll_copy_permission_and_source_reset() {
    for case in [
        Case::EnabledSourceReset,
        Case::Disabled,
        Case::OldBaseAfterReset,
    ] {
        let enabled = !matches!(case, Case::Disabled);
        let first = pair(1);
        let mut records = Vec::new();
        let mut expected = Vec::new();
        let pool = new_decode_buffer_pool();
        let mut reference = TileDecoder::new();
        reference.set_scroll_copy_enabled(true);
        for payload in &first {
            records.push((
                MessageKind::Frame,
                encode_source_payload(1, payload).unwrap(),
            ));
            if enabled || expected.is_empty() {
                let decoded = reference.decode_into(payload, Arc::clone(&pool)).unwrap();
                expected.push((1, decoded.sequence(), decoded.image().as_raw().clone()));
            }
        }
        if enabled {
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
                let keyframe = pair(3).remove(0);
                reference = TileDecoder::new();
                reference.set_scroll_copy_enabled(true);
                let decoded = reference.decode_into(&keyframe, Arc::clone(&pool)).unwrap();
                expected.push((2, decoded.sequence(), decoded.image().as_raw().clone()));
                records.push((
                    MessageKind::Frame,
                    encode_source_payload(2, &keyframe).unwrap(),
                ));
                let mut old = first[1].clone();
                old[..8].copy_from_slice(&4u64.to_be_bytes());
                records.push((MessageKind::Frame, encode_source_payload(2, &old).unwrap()));
            } else {
                reference = TileDecoder::new();
                reference.set_scroll_copy_enabled(true);
                for payload in pair(3) {
                    let decoded = reference.decode_into(&payload, Arc::clone(&pool)).unwrap();
                    expected.push((2, decoded.sequence(), decoded.image().as_raw().clone()));
                    records.push((
                        MessageKind::Frame,
                        encode_source_payload(2, &payload).unwrap(),
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
            assert!(
                error
                    .to_string()
                    .contains("scroll copy capability or base mismatch"),
                "case={case:?}: {error}"
            );
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
