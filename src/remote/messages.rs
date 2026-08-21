//! Small, capability-limited application messages carried by the authenticated
//! remote transport.  This is intentionally unrelated to JWM's local IPC.

use super::RemoteResult;
use super::protocol::MessageKind;
use super::x11_input::InputEvent;
use std::io;

const APPLICATION_MAGIC: &[u8; 8] = b"JWMREM01";
const APPLICATION_VERSION: u16 = 4;
const SERVER_HELLO_LEN: usize = 11;
const CLIENT_HELLO_LEN: usize = SERVER_HELLO_LEN + 32;
const FLAG_POINTER: u8 = 1 << 0;
const FLAG_KEYBOARD: u8 = 1 << 1;
const KNOWN_FLAGS: u8 = FLAG_POINTER | FLAG_KEYBOARD;
const INPUT_TAG_POINTER: u8 = 1;
const INPUT_TAG_KEY: u8 = 2;
const INPUT_TAG_BUTTON: u8 = 3;
const INPUT_TAG_RELEASE_ALL: u8 = 4;
const POINTER_INPUT_LEN: usize = 1 + 2 + 2;
const MIN_X11_KEYCODE: u8 = 8;

/// Maximum number of input operations carried by one [`MessageKind::InputBatch`].
pub const MAX_INPUT_BATCH_EVENTS: usize = 128;

/// Independent payload bound for [`MessageKind::InputBatch`].
///
/// A full batch of pointer operations is the largest valid representation:
/// one count byte followed by 128 five-byte operations.
pub const MAX_INPUT_BATCH_PAYLOAD_LEN: usize = 1 + MAX_INPUT_BATCH_EVENTS * POINTER_INPUT_LEN;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClientHello {
    pub request_input: bool,
    pub keymap_fingerprint: [u8; 32],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ServerHello {
    pub pointer_enabled: bool,
    pub keyboard_enabled: bool,
}

impl ClientHello {
    #[must_use]
    pub fn encode(self) -> [u8; CLIENT_HELLO_LEN] {
        let mut payload = [0_u8; CLIENT_HELLO_LEN];
        let flags = if self.request_input { KNOWN_FLAGS } else { 0 };
        payload[..SERVER_HELLO_LEN].copy_from_slice(&encode_hello(flags));
        payload[SERVER_HELLO_LEN..].copy_from_slice(&self.keymap_fingerprint);
        payload
    }

    pub fn decode(payload: &[u8]) -> RemoteResult<Self> {
        if payload.len() != CLIENT_HELLO_LEN {
            return Err(invalid_data("client hello has an invalid length").into());
        }
        let request_input = decode_hello(&payload[..SERVER_HELLO_LEN])? != 0;
        let keymap_fingerprint = payload[SERVER_HELLO_LEN..].try_into().unwrap();
        Ok(Self {
            request_input,
            keymap_fingerprint,
        })
    }
}

impl ServerHello {
    #[must_use]
    pub fn encode(self) -> [u8; SERVER_HELLO_LEN] {
        let mut flags = 0;
        if self.pointer_enabled {
            flags |= FLAG_POINTER;
        }
        if self.keyboard_enabled {
            flags |= FLAG_KEYBOARD;
        }
        encode_hello(flags)
    }

    pub fn decode(payload: &[u8]) -> RemoteResult<Self> {
        let flags = decode_hello(payload)?;
        Ok(Self {
            pointer_enabled: flags & FLAG_POINTER != 0,
            keyboard_enabled: flags & FLAG_KEYBOARD != 0,
        })
    }
}

fn encode_hello(flags: u8) -> [u8; SERVER_HELLO_LEN] {
    let mut payload = [0_u8; SERVER_HELLO_LEN];
    payload[..8].copy_from_slice(APPLICATION_MAGIC);
    payload[8..10].copy_from_slice(&APPLICATION_VERSION.to_be_bytes());
    payload[10] = flags;
    payload
}

fn decode_hello(payload: &[u8]) -> RemoteResult<u8> {
    if payload.len() != SERVER_HELLO_LEN || &payload[..8] != APPLICATION_MAGIC {
        return Err(invalid_data("peer is not using the JWM remote protocol").into());
    }
    let version = u16::from_be_bytes(payload[8..10].try_into().unwrap());
    if version != APPLICATION_VERSION {
        return Err(invalid_data(format!(
            "unsupported JWM remote application version {version}; expected {APPLICATION_VERSION}"
        ))
        .into());
    }
    if payload[10] & !KNOWN_FLAGS != 0 {
        return Err(invalid_data("peer advertised unknown remote capabilities").into());
    }
    Ok(payload[10])
}

/// Encode a cumulative acknowledgement for the latest frame drawn by the client.
#[must_use]
pub fn encode_frame_ack(sequence: u64) -> [u8; 8] {
    sequence.to_be_bytes()
}

/// Decode a frame acknowledgement, rejecting anything other than one `u64`.
pub fn decode_frame_ack(payload: &[u8]) -> RemoteResult<u64> {
    let sequence = payload
        .try_into()
        .map_err(|_| invalid_data("frame acknowledgement has an invalid length"))?;
    Ok(u64::from_be_bytes(sequence))
}

#[must_use]
/// Encode the transitional single-event representation.
///
/// Version-3 network peers use [`encode_input_batch`] instead. This helper is
/// retained while callers migrate without changing the stable legacy kinds.
pub fn encode_input(event: InputEvent) -> (MessageKind, Vec<u8>) {
    match event {
        InputEvent::Pointer { x, y } => {
            let mut payload = Vec::with_capacity(4);
            payload.extend_from_slice(&x.to_be_bytes());
            payload.extend_from_slice(&y.to_be_bytes());
            (MessageKind::Pointer, payload)
        }
        InputEvent::Key { keycode, pressed } => {
            (MessageKind::Key, vec![keycode, u8::from(pressed)])
        }
        InputEvent::Button { button, pressed } => {
            (MessageKind::Button, vec![button, u8::from(pressed)])
        }
        InputEvent::ReleaseAll => (MessageKind::ReleaseAll, Vec::new()),
    }
}

/// Decode the transitional single-event representation.
///
/// Version-3 network receivers should accept [`MessageKind::InputBatch`] and
/// reject these legacy kinds rather than exposing two validation policies.
pub fn decode_input(kind: MessageKind, payload: &[u8]) -> RemoteResult<InputEvent> {
    match (kind, payload) {
        (MessageKind::Pointer, [x0, x1, y0, y1]) => Ok(InputEvent::Pointer {
            x: u16::from_be_bytes([*x0, *x1]),
            y: u16::from_be_bytes([*y0, *y1]),
        }),
        (MessageKind::Key, [keycode, pressed @ (0 | 1)]) => Ok(InputEvent::Key {
            keycode: *keycode,
            pressed: *pressed != 0,
        }),
        (MessageKind::Button, [button, pressed @ (0 | 1)]) if *button != 0 => {
            Ok(InputEvent::Button {
                button: *button,
                pressed: *pressed != 0,
            })
        }
        (MessageKind::ReleaseAll, []) => Ok(InputEvent::ReleaseAll),
        _ => Err(invalid_data("malformed remote input message").into()),
    }
}

/// Encode input operations as one version-3 atomic input batch.
///
/// Every operation is validated before any output allocation is exposed. An
/// empty batch and batches larger than [`MAX_INPUT_BATCH_EVENTS`] are rejected.
pub fn encode_input_batch(events: &[InputEvent]) -> RemoteResult<Vec<u8>> {
    let mut payload = Vec::new();
    encode_input_batch_into(events, &mut payload)?;
    Ok(payload)
}

/// Encode an input batch into a reusable payload allocation.
///
/// `output` is cleared on entry and after every error. Validation of the whole
/// batch precedes reserving or writing output bytes, so an invalid final event
/// never leaves a visible encoded prefix.
pub fn encode_input_batch_into(events: &[InputEvent], output: &mut Vec<u8>) -> RemoteResult<()> {
    output.clear();
    let result = (|| {
        if events.is_empty() || events.len() > MAX_INPUT_BATCH_EVENTS {
            return Err(invalid_data(format!(
                "remote input batch contains {} events; expected 1..={MAX_INPUT_BATCH_EVENTS}",
                events.len()
            ))
            .into());
        }

        let mut payload_len = 1_usize;
        for &event in events {
            validate_batch_event(event)?;
            payload_len += batch_event_len(event);
        }
        if payload_len > MAX_INPUT_BATCH_PAYLOAD_LEN {
            return Err(invalid_data("remote input batch exceeds its payload limit").into());
        }

        output.reserve(payload_len);
        output.push(
            u8::try_from(events.len()).expect("the validated input batch count always fits in u8"),
        );
        for &event in events {
            match event {
                InputEvent::Pointer { x, y } => {
                    output.push(INPUT_TAG_POINTER);
                    output.extend_from_slice(&x.to_be_bytes());
                    output.extend_from_slice(&y.to_be_bytes());
                }
                InputEvent::Key { keycode, pressed } => {
                    output.extend_from_slice(&[INPUT_TAG_KEY, keycode, u8::from(pressed)]);
                }
                InputEvent::Button { button, pressed } => {
                    output.extend_from_slice(&[INPUT_TAG_BUTTON, button, u8::from(pressed)]);
                }
                InputEvent::ReleaseAll => output.push(INPUT_TAG_RELEASE_ALL),
            }
        }
        debug_assert_eq!(output.len(), payload_len);
        Ok(())
    })();
    if result.is_err() {
        output.clear();
    }
    result
}

/// Decode and validate one complete version-3 atomic input batch.
///
/// Parsing happens into a private vector and succeeds only when all declared
/// operations and the payload boundary are valid. Callers can therefore never
/// observe a valid prefix from a malformed batch.
pub fn decode_input_batch(payload: &[u8]) -> RemoteResult<Vec<InputEvent>> {
    if payload.is_empty() || payload.len() > MAX_INPUT_BATCH_PAYLOAD_LEN {
        return Err(invalid_data(format!(
            "remote input batch payload has invalid length {}; maximum is {MAX_INPUT_BATCH_PAYLOAD_LEN}",
            payload.len()
        ))
        .into());
    }

    let count = usize::from(payload[0]);
    if count == 0 || count > MAX_INPUT_BATCH_EVENTS {
        return Err(invalid_data(format!(
            "remote input batch contains {count} events; expected 1..={MAX_INPUT_BATCH_EVENTS}"
        ))
        .into());
    }

    let mut events = Vec::with_capacity(count);
    let mut offset = 1;
    for _ in 0..count {
        let tag = take_byte(payload, &mut offset)?;
        let event = match tag {
            INPUT_TAG_POINTER => {
                let x = take_u16(payload, &mut offset)?;
                let y = take_u16(payload, &mut offset)?;
                InputEvent::Pointer { x, y }
            }
            INPUT_TAG_KEY => {
                let keycode = take_byte(payload, &mut offset)?;
                let pressed = decode_edge(take_byte(payload, &mut offset)?)?;
                InputEvent::Key { keycode, pressed }
            }
            INPUT_TAG_BUTTON => {
                let button = take_byte(payload, &mut offset)?;
                let pressed = decode_edge(take_byte(payload, &mut offset)?)?;
                InputEvent::Button { button, pressed }
            }
            INPUT_TAG_RELEASE_ALL => InputEvent::ReleaseAll,
            _ => return Err(invalid_data("remote input batch contains an unknown tag").into()),
        };
        validate_batch_event(event)?;
        events.push(event);
    }

    if offset != payload.len() {
        return Err(invalid_data("remote input batch has trailing bytes").into());
    }
    Ok(events)
}

fn batch_event_len(event: InputEvent) -> usize {
    match event {
        InputEvent::Pointer { .. } => POINTER_INPUT_LEN,
        InputEvent::Key { .. } | InputEvent::Button { .. } => 3,
        InputEvent::ReleaseAll => 1,
    }
}

fn validate_batch_event(event: InputEvent) -> RemoteResult<()> {
    match event {
        InputEvent::Pointer { x, y } if x > i16::MAX as u16 || y > i16::MAX as u16 => {
            Err(invalid_data("remote input batch pointer coordinate is out of range").into())
        }
        InputEvent::Key { keycode, .. } if keycode < MIN_X11_KEYCODE => {
            Err(invalid_data("remote input batch X11 keycode is out of range").into())
        }
        InputEvent::Button { button: 0, .. } => {
            Err(invalid_data("remote input batch button is zero").into())
        }
        _ => Ok(()),
    }
}

fn take_byte(payload: &[u8], offset: &mut usize) -> RemoteResult<u8> {
    let value = payload
        .get(*offset)
        .copied()
        .ok_or_else(|| invalid_data("remote input batch is truncated"))?;
    *offset += 1;
    Ok(value)
}

fn take_u16(payload: &[u8], offset: &mut usize) -> RemoteResult<u16> {
    let high = take_byte(payload, offset)?;
    let low = take_byte(payload, offset)?;
    Ok(u16::from_be_bytes([high, low]))
}

fn decode_edge(edge: u8) -> RemoteResult<bool> {
    match edge {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(invalid_data("remote input batch edge is neither pressed nor released").into()),
    }
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_is_versioned_and_rejects_unknown_flags() {
        let payload = ClientHello {
            request_input: true,
            keymap_fingerprint: [7; 32],
        }
        .encode();
        let decoded = ClientHello::decode(&payload).unwrap();
        assert!(decoded.request_input);
        assert_eq!(decoded.keymap_fingerprint, [7; 32]);

        let mut future = payload;
        future[10] |= 0x80;
        assert!(ClientHello::decode(&future).is_err());
    }

    #[test]
    fn hello_uses_version_four_and_rejects_neighbouring_versions() {
        let payload = ServerHello {
            pointer_enabled: false,
            keyboard_enabled: false,
        }
        .encode();
        assert_eq!(&payload[8..10], &4_u16.to_be_bytes());

        // Version 3 carried whole-frame JPEGs; version 4 carries dirty-tile
        // deltas on the same message kind, so an older peer must fail the
        // handshake rather than misread the first frame body.
        let mut version_three = payload;
        version_three[8..10].copy_from_slice(&3_u16.to_be_bytes());
        let error = ServerHello::decode(&version_three).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("application version 3; expected 4")
        );

        for rejected in [0_u16, 2, 5, u16::MAX] {
            let mut other = payload;
            other[8..10].copy_from_slice(&rejected.to_be_bytes());
            assert!(
                ServerHello::decode(&other).is_err(),
                "version {rejected} must be rejected"
            );
        }
    }

    #[test]
    fn frame_ack_round_trips_and_requires_exact_length() {
        for sequence in [0, 1, u64::MAX] {
            let payload = encode_frame_ack(sequence);
            assert_eq!(payload, sequence.to_be_bytes());
            assert_eq!(decode_frame_ack(&payload).unwrap(), sequence);
        }

        assert!(decode_frame_ack(&[0; 7]).is_err());
        assert!(decode_frame_ack(&[0; 9]).is_err());
    }

    #[test]
    fn input_messages_round_trip_and_reject_invalid_edges() {
        let events = [
            InputEvent::Pointer { x: 321, y: 654 },
            InputEvent::Key {
                keycode: 38,
                pressed: true,
            },
            InputEvent::Button {
                button: 1,
                pressed: false,
            },
            InputEvent::ReleaseAll,
        ];
        for event in events {
            let (kind, payload) = encode_input(event);
            assert_eq!(decode_input(kind, &payload).unwrap(), event);
        }
        assert!(decode_input(MessageKind::Key, &[38, 2]).is_err());
        assert!(decode_input(MessageKind::Button, &[0, 1]).is_err());
    }

    #[test]
    fn input_batch_round_trips_all_event_types() {
        let events = [
            InputEvent::Pointer { x: 0, y: 32767 },
            InputEvent::Key {
                keycode: 38,
                pressed: true,
            },
            InputEvent::Button {
                button: 1,
                pressed: false,
            },
            InputEvent::ReleaseAll,
        ];
        let payload = encode_input_batch(&events).unwrap();
        assert_eq!(payload, [4, 1, 0, 0, 0x7f, 0xff, 2, 38, 1, 3, 1, 0, 4,]);
        assert_eq!(decode_input_batch(&payload).unwrap(), events);
    }

    #[test]
    fn input_batch_accepts_exact_event_and_payload_limits() {
        let events = vec![
            InputEvent::Pointer {
                x: i16::MAX as u16,
                y: i16::MAX as u16,
            };
            MAX_INPUT_BATCH_EVENTS
        ];
        let payload = encode_input_batch(&events).unwrap();
        assert_eq!(payload.len(), MAX_INPUT_BATCH_PAYLOAD_LEN);
        assert_eq!(usize::from(payload[0]), MAX_INPUT_BATCH_EVENTS);
        assert_eq!(decode_input_batch(&payload).unwrap(), events);
    }

    #[test]
    fn input_batch_accepts_x11_keycode_and_button_boundaries() {
        let events = [
            InputEvent::Key {
                keycode: MIN_X11_KEYCODE,
                pressed: false,
            },
            InputEvent::Key {
                keycode: u8::MAX,
                pressed: true,
            },
            InputEvent::Button {
                button: u8::MAX,
                pressed: true,
            },
        ];
        let payload = encode_input_batch(&events).unwrap();
        assert_eq!(decode_input_batch(&payload).unwrap(), events);
    }

    #[test]
    fn input_batch_encoder_reuses_output_and_clears_every_error() {
        let mut payload = Vec::with_capacity(MAX_INPUT_BATCH_PAYLOAD_LEN);
        payload.extend_from_slice(b"stale");
        let allocation = payload.as_ptr();
        let capacity = payload.capacity();

        encode_input_batch_into(&[InputEvent::ReleaseAll], &mut payload).unwrap();
        assert_eq!(payload, [1, INPUT_TAG_RELEASE_ALL]);
        assert_eq!(payload.as_ptr(), allocation);
        assert_eq!(payload.capacity(), capacity);

        encode_input_batch_into(
            &[InputEvent::Key {
                keycode: 38,
                pressed: true,
            }],
            &mut payload,
        )
        .unwrap();
        assert_eq!(payload, [1, INPUT_TAG_KEY, 38, 1]);
        assert_eq!(payload.as_ptr(), allocation);
        assert_eq!(payload.capacity(), capacity);

        assert!(
            encode_input_batch_into(
                &[
                    InputEvent::ReleaseAll,
                    InputEvent::Button {
                        button: 0,
                        pressed: true,
                    },
                ],
                &mut payload,
            )
            .is_err()
        );
        assert!(payload.is_empty());
        assert_eq!(payload.as_ptr(), allocation);
        assert_eq!(payload.capacity(), capacity);
    }

    #[test]
    fn input_batch_rejects_empty_oversized_and_inconsistent_boundaries() {
        assert!(encode_input_batch(&[]).is_err());
        assert!(decode_input_batch(&[]).is_err());

        let too_many = vec![InputEvent::ReleaseAll; MAX_INPUT_BATCH_EVENTS + 1];
        assert!(encode_input_batch(&too_many).is_err());
        let mut too_many_payload = vec![(MAX_INPUT_BATCH_EVENTS + 1) as u8];
        too_many_payload.extend(std::iter::repeat_n(
            INPUT_TAG_RELEASE_ALL,
            MAX_INPUT_BATCH_EVENTS + 1,
        ));
        assert!(decode_input_batch(&too_many_payload).is_err());

        let oversized_payload = vec![0; MAX_INPUT_BATCH_PAYLOAD_LEN + 1];
        assert!(decode_input_batch(&oversized_payload).is_err());
        assert!(decode_input_batch(&[0]).is_err());
        assert!(decode_input_batch(&[1, INPUT_TAG_RELEASE_ALL, 0]).is_err());
        assert!(decode_input_batch(&[2, INPUT_TAG_RELEASE_ALL]).is_err());
    }

    #[test]
    fn input_batch_rejects_unknown_tags_truncation_and_invalid_fields() {
        for malformed in [
            vec![1, 0xff],
            vec![1, INPUT_TAG_POINTER],
            vec![1, INPUT_TAG_POINTER, 0, 1, 0],
            vec![1, INPUT_TAG_KEY],
            vec![1, INPUT_TAG_KEY, 38],
            vec![1, INPUT_TAG_BUTTON],
            vec![1, INPUT_TAG_BUTTON, 1],
            vec![1, INPUT_TAG_POINTER, 0x80, 0, 0, 0],
            vec![1, INPUT_TAG_POINTER, 0, 0, 0x80, 0],
            vec![1, INPUT_TAG_KEY, 0, 1],
            vec![1, INPUT_TAG_KEY, 38, 2],
            vec![1, INPUT_TAG_BUTTON, 0, 1],
            vec![1, INPUT_TAG_BUTTON, 1, 2],
        ] {
            assert!(
                decode_input_batch(&malformed).is_err(),
                "accepted malformed batch {malformed:?}"
            );
        }

        for invalid in [
            InputEvent::Pointer { x: 32768, y: 0 },
            InputEvent::Pointer { x: 0, y: 32768 },
            InputEvent::Key {
                keycode: 7,
                pressed: true,
            },
            InputEvent::Button {
                button: 0,
                pressed: false,
            },
        ] {
            assert!(encode_input_batch(&[invalid]).is_err());
        }
    }

    #[test]
    fn malformed_final_batch_event_never_returns_a_valid_prefix() {
        let malformed = [
            3,
            INPUT_TAG_RELEASE_ALL,
            INPUT_TAG_KEY,
            38,
            1,
            INPUT_TAG_BUTTON,
            1,
            2,
        ];
        assert!(decode_input_batch(&malformed).is_err());
    }

    #[test]
    fn every_truncated_mixed_batch_prefix_is_invalid_data() {
        let payload = encode_input_batch(&[
            InputEvent::Pointer { x: 12, y: 34 },
            InputEvent::Key {
                keycode: 38,
                pressed: true,
            },
            InputEvent::Button {
                button: 1,
                pressed: false,
            },
            InputEvent::ReleaseAll,
        ])
        .unwrap();

        for end in 0..payload.len() {
            let error = decode_input_batch(&payload[..end]).unwrap_err();
            assert_eq!(
                error
                    .downcast_ref::<io::Error>()
                    .expect("batch validation errors are io::Error")
                    .kind(),
                io::ErrorKind::InvalidData,
                "unexpected error for prefix length {end}"
            );
        }
    }

    #[test]
    fn server_can_negotiate_pointer_without_keyboard() {
        let payload = ServerHello {
            pointer_enabled: true,
            keyboard_enabled: false,
        }
        .encode();
        let decoded = ServerHello::decode(&payload).unwrap();
        assert!(decoded.pointer_enabled);
        assert!(!decoded.keyboard_enabled);
    }
}
