//! Small, capability-limited application messages carried by the authenticated
//! remote transport.  This is intentionally unrelated to JWM's local IPC.

use super::RemoteResult;
use super::protocol::MessageKind;
use super::x11_input::InputEvent;
use super::x11_keymap::{KeymapTable, MAX_ENCODED_KEYMAP_LEN};
use std::io;

const APPLICATION_MAGIC: &[u8; 8] = b"JWMREM01";
pub const APPLICATION_VERSION: u16 = 9;
const HELLO_PREFIX_LEN: usize = 11;
const STREAM_CAPABILITIES_LEN: usize = 11;
const SERVER_HELLO_LEN: usize = HELLO_PREFIX_LEN + STREAM_CAPABILITIES_LEN;
const CLIENT_HELLO_HEADER_LEN: usize = SERVER_HELLO_LEN + 4;
/// Largest client hello: the shared prefix plus a full keymap table.
pub const MAX_CLIENT_HELLO_LEN: usize = CLIENT_HELLO_HEADER_LEN + MAX_ENCODED_KEYMAP_LEN;
const FLAG_POINTER: u8 = 1 << 0;
const FLAG_KEYBOARD: u8 = 1 << 1;
const FLAG_CLIPBOARD: u8 = 1 << 2;
/// Copied files travel too. Only meaningful together with [`FLAG_CLIPBOARD`].
const FLAG_CLIPBOARD_FILES: u8 = 1 << 3;
const INPUT_FLAGS: u8 = FLAG_POINTER | FLAG_KEYBOARD;
const KNOWN_FLAGS: u8 = INPUT_FLAGS | FLAG_CLIPBOARD | FLAG_CLIPBOARD_FILES;
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

/// The implemented stream profile. Selectors are explicit so a peer cannot
/// silently interpret a future codec, cursor plane or input coordinate space.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StreamCapabilities {
    pub codec: FrameCodec,
    pub cursor: CursorMode,
    pub input: InputMode,
    /// Maximum aggregate copied-file bytes; zero disables files.
    pub clipboard_file_limit: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameCodec {
    #[default]
    JpegTiles = 1,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum CursorMode {
    /// Cursor pixels remain in the captured video, never a second local plane.
    #[default]
    Composited = 1,
    /// Viewer supports independent shape/position records; host may fall back.
    Independent = 2,
}

impl CursorMode {
    /// A viewer offering separation also accepts the safe composited fallback.
    #[must_use]
    pub fn negotiate(self, host_can_separate: bool) -> Self {
        if self == Self::Independent && host_can_separate {
            Self::Independent
        } else {
            Self::Composited
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum InputMode {
    #[default]
    X11Keycodes = 1,
}

impl StreamCapabilities {
    fn encode(self) -> [u8; STREAM_CAPABILITIES_LEN] {
        let mut payload = [0; STREAM_CAPABILITIES_LEN];
        payload[..3].copy_from_slice(&[self.codec as u8, self.cursor as u8, self.input as u8]);
        payload[3..11].copy_from_slice(&self.clipboard_file_limit.to_be_bytes());
        payload
    }

    fn decode(payload: &[u8]) -> RemoteResult<Self> {
        if payload.len() != STREAM_CAPABILITIES_LEN {
            return Err(invalid_data("invalid bounded stream profile length").into());
        }
        let cursor = match &payload[..3] {
            [1, 1, 1] => CursorMode::Composited,
            [1, 2, 1] => CursorMode::Independent,
            _ => return Err(invalid_data("unsupported remote codec, cursor or input mode").into()),
        };
        Ok(Self {
            cursor,
            clipboard_file_limit: u64::from_be_bytes(payload[3..11].try_into().unwrap()),
            ..Self::default()
        })
    }

    /// Both directions use the stricter operator limit. A zero on either side
    /// disables files, without disabling text and image sharing.
    #[must_use]
    pub fn negotiate_file_limit(self, host_limit: u64) -> u64 {
        self.clipboard_file_limit.min(host_limit)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientHello {
    pub stream: StreamCapabilities,
    /// Desired first-frame bounds; None requests the host default.
    pub initial_viewport: Option<(u16, u16)>,
    pub request_input: bool,
    pub request_clipboard: bool,
    /// Also share copied files. Requires `request_clipboard`.
    pub request_clipboard_files: bool,
    /// The viewer's keymap, empty for a view-only viewer. Version 5 replaced
    /// a single whole-map digest with this per-keycode table.
    pub keymap: KeymapTable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ServerHello {
    pub stream: StreamCapabilities,
    pub pointer_enabled: bool,
    pub keyboard_enabled: bool,
    pub clipboard_enabled: bool,
    /// Copied files are shared too. Implies `clipboard_enabled`.
    pub clipboard_files_enabled: bool,
}

impl ClientHello {
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut flags = if self.request_input { INPUT_FLAGS } else { 0 };
        if self.request_clipboard {
            flags |= FLAG_CLIPBOARD;
            if self.request_clipboard_files {
                flags |= FLAG_CLIPBOARD_FILES;
            }
        }
        let mut payload = Vec::with_capacity(MAX_CLIENT_HELLO_LEN);
        payload.extend_from_slice(&encode_hello(flags, self.stream));
        let (width, height) = self.initial_viewport.unwrap_or((0, 0));
        payload.extend_from_slice(&encode_viewport(width, height));
        self.keymap.encode_into(&mut payload);
        payload
    }

    pub fn decode(payload: &[u8]) -> RemoteResult<Self> {
        if payload.len() < HELLO_PREFIX_LEN || payload.len() > MAX_CLIENT_HELLO_LEN {
            return Err(invalid_data("client hello has an invalid length").into());
        }
        // The version is checked before the keymap, so an older viewer is
        // told its version is unsupported rather than that its table is bad.
        let flags = decode_prefix(&payload[..HELLO_PREFIX_LEN])?;
        if payload.len() < CLIENT_HELLO_HEADER_LEN {
            return Err(invalid_data("client hello header is truncated").into());
        }
        let stream = StreamCapabilities::decode(&payload[HELLO_PREFIX_LEN..SERVER_HELLO_LEN])?;
        let viewport = &payload[SERVER_HELLO_LEN..CLIENT_HELLO_HEADER_LEN];
        let initial_viewport = if viewport == [0; 4] {
            None
        } else {
            Some(decode_viewport(viewport)?)
        };
        let keymap = KeymapTable::decode(&payload[CLIENT_HELLO_HEADER_LEN..])?;
        Ok(Self {
            stream,
            initial_viewport,
            request_input: flags & INPUT_FLAGS != 0,
            request_clipboard: flags & FLAG_CLIPBOARD != 0,
            request_clipboard_files: flags & FLAG_CLIPBOARD_FILES != 0,
            keymap,
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
        if self.clipboard_enabled {
            flags |= FLAG_CLIPBOARD;
            if self.clipboard_files_enabled {
                flags |= FLAG_CLIPBOARD_FILES;
            }
        }
        encode_hello(flags, self.stream)
    }

    pub fn decode(payload: &[u8]) -> RemoteResult<Self> {
        if payload.len() < HELLO_PREFIX_LEN {
            return Err(invalid_data("server hello header is truncated").into());
        }
        let flags = decode_prefix(&payload[..HELLO_PREFIX_LEN])?;
        if payload.len() != SERVER_HELLO_LEN {
            return Err(invalid_data("server hello has an invalid length").into());
        }
        let stream = StreamCapabilities::decode(&payload[HELLO_PREFIX_LEN..])?;
        Ok(Self {
            stream,
            pointer_enabled: flags & FLAG_POINTER != 0,
            keyboard_enabled: flags & FLAG_KEYBOARD != 0,
            clipboard_enabled: flags & FLAG_CLIPBOARD != 0,
            clipboard_files_enabled: flags & FLAG_CLIPBOARD_FILES != 0,
        })
    }
}

fn encode_hello(flags: u8, stream: StreamCapabilities) -> [u8; SERVER_HELLO_LEN] {
    let mut payload = [0_u8; SERVER_HELLO_LEN];
    payload[..8].copy_from_slice(APPLICATION_MAGIC);
    payload[8..10].copy_from_slice(&APPLICATION_VERSION.to_be_bytes());
    payload[10] = flags;
    payload[HELLO_PREFIX_LEN..].copy_from_slice(&stream.encode());
    payload
}

fn decode_prefix(payload: &[u8]) -> RemoteResult<u8> {
    if payload.len() != HELLO_PREFIX_LEN || &payload[..8] != APPLICATION_MAGIC {
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
    if payload[10] & FLAG_CLIPBOARD_FILES != 0 && payload[10] & FLAG_CLIPBOARD == 0 {
        return Err(invalid_data("peer advertised file sharing without clipboard sharing").into());
    }
    Ok(payload[10])
}

/// Smallest viewport a client may request, matching the CLI's `--max-width`
/// floor so a peer cannot drive the encoder below a supported size.
pub const MIN_VIEWPORT_WIDTH: u16 = 320;

/// Smallest viewport height a client may request.
pub const MIN_VIEWPORT_HEIGHT: u16 = 200;

/// Encode the client's current viewable size.
#[must_use]
pub fn encode_viewport(width: u16, height: u16) -> [u8; 4] {
    let mut payload = [0_u8; 4];
    payload[..2].copy_from_slice(&width.to_be_bytes());
    payload[2..].copy_from_slice(&height.to_be_bytes());
    payload
}

/// Decode a viewport request.
///
/// Only the shape is validated here. What the host is willing to *do* with the
/// request stays a host policy decision, because the operator's `--max-width`
/// remains the ceiling.
pub fn decode_viewport(payload: &[u8]) -> RemoteResult<(u16, u16)> {
    if payload.len() != 4 {
        return Err(invalid_data("viewport payload must be exactly four bytes").into());
    }
    let width = u16::from_be_bytes(payload[..2].try_into().unwrap());
    let height = u16::from_be_bytes(payload[2..].try_into().unwrap());
    if width == 0 || height == 0 {
        return Err(invalid_data("viewport has an empty dimension").into());
    }
    Ok((width, height))
}

/// Largest clipboard payload carried in one record.
///
/// Matches the local clipboard history's own ceiling, so a selection this
/// helper refuses is exactly one the rest of JWM would refuse too.
pub const MAX_CLIPBOARD_BYTES: usize = crate::backend::clipboard_offer::MAX_TEXT_BYTES;

/// Validate a clipboard payload for the wire.
pub fn encode_clipboard(text: &str) -> RemoteResult<&[u8]> {
    if text.len() > MAX_CLIPBOARD_BYTES {
        return Err(invalid_data(format!(
            "clipboard text is {} bytes; maximum is {MAX_CLIPBOARD_BYTES}",
            text.len()
        ))
        .into());
    }
    Ok(text.as_bytes())
}

/// Decode a clipboard payload.
///
/// The length is bounded before the text is examined, and the bytes must be
/// valid UTF-8: a peer does not get to hand the local X server arbitrary
/// bytes to hold as a text selection.
pub fn decode_clipboard(payload: &[u8]) -> RemoteResult<String> {
    if payload.len() > MAX_CLIPBOARD_BYTES {
        return Err(invalid_data(format!(
            "clipboard payload is {} bytes; maximum is {MAX_CLIPBOARD_BYTES}",
            payload.len()
        ))
        .into());
    }
    String::from_utf8(payload.to_vec())
        .map_err(|_| invalid_data("clipboard payload is not valid UTF-8").into())
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
    fn bounded_profile_round_trips_and_rejects_truncation() {
        let stream = StreamCapabilities {
            clipboard_file_limit: u64::MAX,
            ..Default::default()
        };
        let payload = stream.encode();
        assert_eq!(StreamCapabilities::decode(&payload).unwrap(), stream);
        for end in 0..payload.len() {
            assert!(StreamCapabilities::decode(&payload[..end]).is_err());
        }
    }

    #[test]
    fn file_budget_is_the_minimum_of_both_peers_including_disabled() {
        for (viewer, host, expected) in [
            (0, 100, 0),
            (100, 0, 0),
            (100, 50, 50),
            (50, 100, 50),
            (u64::MAX, 42, 42),
        ] {
            let stream = StreamCapabilities {
                clipboard_file_limit: viewer,
                ..Default::default()
            };
            assert_eq!(stream.negotiate_file_limit(host), expected);
        }
    }

    #[test]
    fn cursor_capability_is_explicit_and_has_a_composited_fallback() {
        for (request, available, expected) in [
            (CursorMode::Composited, false, CursorMode::Composited),
            (CursorMode::Composited, true, CursorMode::Composited),
            (CursorMode::Independent, false, CursorMode::Composited),
            (CursorMode::Independent, true, CursorMode::Independent),
        ] {
            assert_eq!(request.negotiate(available), expected);
            let stream = StreamCapabilities {
                cursor: request,
                ..Default::default()
            };
            assert_eq!(
                StreamCapabilities::decode(&stream.encode()).unwrap(),
                stream
            );
        }
    }

    #[test]
    fn initial_viewport_round_trips_and_rejects_half_empty_dimensions() {
        for initial_viewport in [
            None,
            Some((320, 200)),
            Some((1920, 1080)),
            Some((u16::MAX, u16::MAX)),
        ] {
            let hello = ClientHello {
                stream: Default::default(),
                initial_viewport,
                request_input: false,
                request_clipboard: false,
                request_clipboard_files: false,
                keymap: KeymapTable::empty(),
            };
            let payload = hello.encode();
            assert_eq!(ClientHello::decode(&payload).unwrap(), hello);
            for dimensions in [(0, 200), (320, 0)] {
                let mut malformed = payload.clone();
                malformed[SERVER_HELLO_LEN..CLIENT_HELLO_HEADER_LEN]
                    .copy_from_slice(&encode_viewport(dimensions.0, dimensions.1));
                assert!(ClientHello::decode(&malformed).is_err());
            }
            for end in 0..payload.len() {
                assert!(ClientHello::decode(&payload[..end]).is_err());
            }
        }
    }

    #[test]
    fn hello_refuses_unimplemented_profiles_before_any_stream_work() {
        let client = ClientHello {
            stream: Default::default(),
            initial_viewport: Some((1920, 1080)),
            request_input: false,
            request_clipboard: false,
            request_clipboard_files: false,
            keymap: KeymapTable::empty(),
        }
        .encode();
        let server = ServerHello {
            stream: Default::default(),
            pointer_enabled: false,
            keyboard_enabled: false,
            clipboard_enabled: false,
            clipboard_files_enabled: false,
        }
        .encode();
        for offset in HELLO_PREFIX_LEN..HELLO_PREFIX_LEN + 3 {
            for unsupported in [0, 3, u8::MAX] {
                let mut client = client.clone();
                client[offset] = unsupported;
                assert!(ClientHello::decode(&client).is_err());
                let mut server = server;
                server[offset] = unsupported;
                assert!(ServerHello::decode(&server).is_err());
            }
        }
        // A true old wire prefix, not a v9-sized payload with only its version
        // changed, still produces the actionable version error.
        let mut legacy = server[..HELLO_PREFIX_LEN].to_vec();
        legacy[8..10].copy_from_slice(&6_u16.to_be_bytes());
        for error in [
            ClientHello::decode(&legacy).unwrap_err(),
            ServerHello::decode(&legacy).unwrap_err(),
        ] {
            assert!(error.to_string().contains("version 6; expected 9"));
        }
    }

    #[test]
    fn hello_is_versioned_and_rejects_unknown_flags() {
        let payload = ClientHello {
            stream: Default::default(),
            initial_viewport: None,
            request_input: true,
            request_clipboard: false,
            request_clipboard_files: false,
            keymap: KeymapTable::empty(),
        }
        .encode();
        let decoded = ClientHello::decode(&payload).unwrap();
        assert!(decoded.request_input);
        assert!(!decoded.request_clipboard);
        assert!(decoded.keymap.is_empty());

        let mut future = payload;
        future[10] |= 0x80;
        assert!(ClientHello::decode(&future).is_err());
    }

    #[test]
    fn clipboard_capability_is_independent_of_input() {
        // A view-only viewer may still share a clipboard, and an input-capable
        // one may decline to. Neither implies the other.
        for (request_input, request_clipboard) in
            [(false, false), (false, true), (true, false), (true, true)]
        {
            let decoded = ClientHello::decode(
                &ClientHello {
                    stream: Default::default(),
                    initial_viewport: None,
                    request_input,
                    request_clipboard,
                    request_clipboard_files: request_clipboard,
                    keymap: KeymapTable::empty(),
                }
                .encode(),
            )
            .unwrap();
            assert_eq!(decoded.request_input, request_input);
            assert_eq!(decoded.request_clipboard, request_clipboard);
            assert_eq!(decoded.request_clipboard_files, request_clipboard);
        }

        for (pointer, keyboard, clipboard) in [
            (false, false, true),
            (true, false, true),
            (true, true, false),
            (false, false, false),
        ] {
            let decoded = ServerHello::decode(
                &ServerHello {
                    stream: Default::default(),
                    pointer_enabled: pointer,
                    keyboard_enabled: keyboard,
                    clipboard_enabled: clipboard,
                    clipboard_files_enabled: false,
                }
                .encode(),
            )
            .unwrap();
            assert_eq!(decoded.pointer_enabled, pointer);
            assert_eq!(decoded.keyboard_enabled, keyboard);
            assert_eq!(decoded.clipboard_enabled, clipboard);
        }
    }

    #[test]
    fn a_viewer_keymap_fits_the_host_inbound_ceiling_and_round_trips() {
        // A full table: every keycode from 8 to 255 bound.
        let mut wire = vec![8, 248];
        wire.extend([0; 32]);
        for keycode in 0..248_u8 {
            wire.extend([keycode, 1, 2]);
        }
        let keymap = KeymapTable::decode(&wire).unwrap();
        let hello = ClientHello {
            stream: Default::default(),
            initial_viewport: None,
            request_input: true,
            request_clipboard: true,
            request_clipboard_files: false,
            keymap,
        };
        let payload = hello.encode();
        assert_eq!(payload.len(), MAX_CLIENT_HELLO_LEN);
        assert!(
            payload.len() <= 1024,
            "host reads at most 1 KiB before negotiation"
        );
        assert_eq!(ClientHello::decode(&payload).unwrap(), hello);
        assert!(ClientHello::decode(&payload[..payload.len() - 1]).is_err());
    }

    #[test]
    fn hello_uses_version_nine_and_rejects_neighbouring_versions() {
        let payload = ServerHello {
            stream: Default::default(),
            pointer_enabled: false,
            keyboard_enabled: false,
            clipboard_enabled: false,
            clipboard_files_enabled: false,
        }
        .encode();
        assert_eq!(&payload[8..10], &9_u16.to_be_bytes());

        // Version 5 had no clipboard transfer records: a version-5 peer
        // would end the session on the first copied image or file. It must
        // fail on its version instead. Version 4 sent one digest of the whole
        // keymap where later versions send a per-keycode table.
        for older in [4_u16, 5, 6, 7, 8] {
            let mut previous = payload;
            previous[8..10].copy_from_slice(&older.to_be_bytes());
            let error = ServerHello::decode(&previous).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains(&format!("application version {older}; expected 9"))
            );
        }

        for rejected in [0_u16, 2, 3, 10, u16::MAX] {
            let mut other = payload;
            other[8..10].copy_from_slice(&rejected.to_be_bytes());
            assert!(
                ServerHello::decode(&other).is_err(),
                "version {rejected} must be rejected"
            );
        }
    }

    #[test]
    fn file_sharing_is_negotiated_only_alongside_clipboard_sharing() {
        let both = ServerHello {
            stream: Default::default(),
            pointer_enabled: false,
            keyboard_enabled: false,
            clipboard_enabled: true,
            clipboard_files_enabled: true,
        };
        assert_eq!(ServerHello::decode(&both.encode()).unwrap(), both);
        // Files without the clipboard are never encoded...
        let files_only = ServerHello {
            clipboard_enabled: false,
            ..both
        };
        assert!(
            !ServerHello::decode(&files_only.encode())
                .unwrap()
                .clipboard_files_enabled
        );
        let hello = ClientHello {
            stream: Default::default(),
            initial_viewport: None,
            request_input: false,
            request_clipboard: false,
            request_clipboard_files: true,
            keymap: KeymapTable::empty(),
        };
        assert!(
            !ClientHello::decode(&hello.encode())
                .unwrap()
                .request_clipboard_files
        );
        // ...and a peer that sends them anyway is refused.
        let mut forged = both.encode();
        forged[10] = FLAG_CLIPBOARD_FILES;
        assert!(ServerHello::decode(&forged).is_err());
    }

    #[test]
    fn clipboard_payloads_round_trip_and_stay_bounded() {
        for text in ["", "hello", "\u{4f60}\u{597d}", "line\nbreak\ttab"] {
            let payload = encode_clipboard(text).unwrap();
            assert_eq!(decode_clipboard(payload).unwrap(), text);
        }

        let oversized = "x".repeat(MAX_CLIPBOARD_BYTES + 1);
        assert!(
            encode_clipboard(&oversized).is_err(),
            "an oversized selection must be refused before it reaches the wire"
        );
        assert!(
            decode_clipboard(oversized.as_bytes()).is_err(),
            "and refused again on arrival, before it is handed to the X server"
        );

        // At the limit exactly, both directions still work.
        let exact = "y".repeat(MAX_CLIPBOARD_BYTES);
        let payload = encode_clipboard(&exact).unwrap();
        assert_eq!(
            decode_clipboard(payload).unwrap().len(),
            MAX_CLIPBOARD_BYTES
        );
    }

    #[test]
    fn a_peer_cannot_hand_the_local_x_server_arbitrary_bytes() {
        // The local side takes CLIPBOARD ownership and serves this as text,
        // so it has to be text.
        assert!(decode_clipboard(&[0xff, 0xfe, 0xfd]).is_err());
        assert!(decode_clipboard(&[b'o', b'k', 0x80]).is_err());
        assert_eq!(decode_clipboard(b"plain ascii").unwrap(), "plain ascii");
    }

    #[test]
    fn viewport_round_trips_and_rejects_malformed_payloads() {
        for (width, height) in [(320_u16, 200_u16), (1280, 720), (u16::MAX, u16::MAX)] {
            let payload = encode_viewport(width, height);
            assert_eq!(decode_viewport(&payload).unwrap(), (width, height));
        }

        assert!(decode_viewport(&[]).is_err(), "empty payload");
        assert!(decode_viewport(&[0; 3]).is_err(), "short payload");
        assert!(decode_viewport(&[0; 5]).is_err(), "trailing bytes");
        assert!(
            decode_viewport(&encode_viewport(0, 100)).is_err(),
            "an empty dimension is not a viewport"
        );
        assert!(decode_viewport(&encode_viewport(100, 0)).is_err());
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
            stream: Default::default(),
            pointer_enabled: true,
            keyboard_enabled: false,
            clipboard_enabled: false,
            clipboard_files_enabled: false,
        }
        .encode();
        let decoded = ServerHello::decode(&payload).unwrap();
        assert!(decoded.pointer_enabled);
        assert!(!decoded.keyboard_enabled);
    }
}
