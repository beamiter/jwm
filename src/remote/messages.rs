//! Small, capability-limited application messages carried by the authenticated
//! remote transport.  This is intentionally unrelated to JWM's local IPC.

use super::RemoteResult;
use super::protocol::MessageKind;
use super::x11_input::InputEvent;
use super::x11_keymap::{KeymapTable, MAX_ENCODED_KEYMAP_LEN};
use std::io;

const APPLICATION_MAGIC: &[u8; 8] = b"JWMREM01";
/// v31 adds explicit isolated-seat popup pointer authorization and family proof records.
/// Native action receipts and proof-gated repair phases remain mandatory.
/// Both peers must upgrade; no fallback to an older authorization contract.
pub const APPLICATION_VERSION: u16 = 31;
const HELLO_PREFIX_LEN: usize = 11;
const STREAM_CAPABILITIES_LEN: usize = 34;
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

/// Hard decoded-canvas ceilings, independent of the desired viewport.
/// Source geometry and padded JPEG atlases retain their separate protocol bounds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameLimits {
    pub max_width: u16,
    pub max_height: u16,
    pub max_pixels: u64,
}

impl Default for FrameLimits {
    fn default() -> Self {
        Self {
            max_width: super::frame::MAX_DIMENSION,
            max_height: super::frame::MAX_DIMENSION,
            max_pixels: super::frame::MAX_PIXELS,
        }
    }
}

impl FrameLimits {
    pub fn validate(self) -> RemoteResult<()> {
        if self.max_width == 0
            || self.max_height == 0
            || self.max_pixels == 0
            || self.max_width > super::frame::MAX_DIMENSION
            || self.max_height > super::frame::MAX_DIMENSION
            || self.max_pixels > super::frame::MAX_PIXELS
        {
            return Err(invalid_data("invalid decoded frame limits").into());
        }
        Ok(())
    }

    #[must_use]
    pub fn intersect(self, other: Self) -> Self {
        Self {
            max_width: self.max_width.min(other.max_width),
            max_height: self.max_height.min(other.max_height),
            max_pixels: self.max_pixels.min(other.max_pixels),
        }
    }

    pub fn validate_dimensions(self, width: u16, height: u16) -> RemoteResult<()> {
        self.validate()?;
        if width == 0
            || height == 0
            || width > self.max_width
            || height > self.max_height
            || u64::from(width) * u64::from(height) > self.max_pixels
        {
            return Err(invalid_data("frame exceeds negotiated decoded frame limits").into());
        }
        Ok(())
    }
}

/// The implemented stream profile. Selectors are explicit so a peer cannot
/// silently interpret a future codec, cursor plane or input coordinate space.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StreamCapabilities {
    pub frame_limits: FrameLimits,
    pub codec: FrameCodec,
    pub cursor: CursorMode,
    pub input: InputMode,
    /// Maximum aggregate copied-file bytes; zero disables files.
    pub clipboard_file_limit: u64,
    /// Content mask: off, text, text/images, or text/images/files.
    pub clipboard_content: u8,
    /// Direction bits: viewer to host (1), host to viewer (2).
    pub clipboard_directions: u8,
    /// Explicit native window resize, independent of input and viewport.
    pub window_resize: bool,
    /// Independent source-bound Wayland keyboard lease; never global input.
    pub wayland_keyboard: bool,
    /// Independent single-action, frame-gated native pointer lease.
    pub wayland_pointer: bool,
    /// Separate opt-in for discrete vertical wheel notches. Requires pointer.
    pub wayland_scroll: bool,
    pub wayland_horizontal_scroll: bool,
    /// Experimental video-only vertical copy deltas; no input authority.
    pub scroll_copy: bool,
    pub static_lossless: bool,
    /// Explicit single combined authority, never inferred from old flags.
    pub wayland_control: bool,
    pub wayland_text: bool,
    pub wayland_popup_pointer: bool,
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
        payload[11..13].copy_from_slice(&self.frame_limits.max_width.to_be_bytes());
        payload[13..15].copy_from_slice(&self.frame_limits.max_height.to_be_bytes());
        payload[15..23].copy_from_slice(&self.frame_limits.max_pixels.to_be_bytes());
        payload[23] = self.clipboard_content;
        payload[24] = self.clipboard_directions;
        payload[25] = u8::from(self.window_resize);
        payload[26] = u8::from(self.wayland_keyboard);
        payload[27] = u8::from(self.wayland_pointer);
        payload[28] =
            u8::from(self.wayland_scroll) | (u8::from(self.wayland_horizontal_scroll) << 1);
        payload[29] = u8::from(self.scroll_copy);
        payload[30] = u8::from(self.wayland_control);
        payload[31] = u8::from(self.wayland_text);
        payload[32] = u8::from(self.static_lossless);
        payload[33] = u8::from(self.wayland_popup_pointer);
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
        if payload[33] > 1
            || (payload[33] == 1 && payload[23..33].iter().any(|b| *b != 0))
            || !matches!(payload[23], 0 | 1 | 3 | 7)
            || payload[24] > 3
            || payload[25] > 1
            || payload[26] > 1
            || payload[27] > 1
            || payload[28] > 3
            || payload[29] > 1
            || payload[32] > 1
            || (payload[32] == 1
                && (payload[23..26].iter().any(|b| *b != 0)
                    || (payload[28] != 0 && payload[27] != 1)
                    || payload[31] != 0))
            || payload[31] > 1
            || (payload[31] == 1 && payload[23..29].iter().any(|b| *b != 0))
            || (payload[31] == 1 && payload[30] != 0)
            || payload[30] > 1
            || (payload[30] == 1
                && (payload[25] != 0
                    || payload[26] != 0
                    || payload[27] != 0
                    || payload[23] != 0
                    || payload[24] != 0))
            || (payload[28] != 0 && (payload[26] == 1 || (payload[27] != 1 && payload[30] != 1)))
            || (payload[26] == 1 && payload[27] == 1)
            || (payload[23] == 0) != (payload[24] == 0)
        {
            return Err(invalid_data("invalid clipboard content or direction policy").into());
        }
        let frame_limits = FrameLimits {
            max_width: u16::from_be_bytes(payload[11..13].try_into().unwrap()),
            max_height: u16::from_be_bytes(payload[13..15].try_into().unwrap()),
            max_pixels: u64::from_be_bytes(payload[15..23].try_into().unwrap()),
        };
        frame_limits.validate()?;
        Ok(Self {
            frame_limits,
            cursor,
            clipboard_file_limit: u64::from_be_bytes(payload[3..11].try_into().unwrap()),
            clipboard_content: payload[23],
            clipboard_directions: payload[24],
            window_resize: payload[25] == 1,
            wayland_keyboard: payload[26] == 1,
            wayland_pointer: payload[27] == 1,
            wayland_scroll: payload[28] & 1 != 0,
            wayland_horizontal_scroll: payload[28] & 2 != 0,
            scroll_copy: payload[29] == 1,
            static_lossless: payload[32] == 1,
            wayland_control: payload[30] == 1,
            wayland_text: payload[31] == 1,
            wayland_popup_pointer: payload[33] == 1,
            ..Self::default()
        })
    }

    /// Video codec extension requires host opt-in and peer decoder support.
    #[must_use]
    pub fn negotiate_scroll_copy(self, host_enabled: bool) -> bool {
        host_enabled && self.scroll_copy
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
        if payload.len() < HELLO_PREFIX_LEN {
            return Err(invalid_data("client hello has an invalid length").into());
        }
        // Check the stable prefix before version-specific size limits. A newer
        // viewer's larger capabilities header can exceed this version's maximum
        // when it sends a full keymap; report the version mismatch in that case.
        let flags = decode_prefix(&payload[..HELLO_PREFIX_LEN])?;
        if payload.len() > MAX_CLIENT_HELLO_LEN {
            return Err(invalid_data("client hello has an invalid length").into());
        }
        if payload.len() < CLIENT_HELLO_HEADER_LEN {
            return Err(invalid_data("client hello header is truncated").into());
        }
        let stream = StreamCapabilities::decode(&payload[HELLO_PREFIX_LEN..SERVER_HELLO_LEN])?;
        validate_clipboard_flags(flags, stream)?;
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
        validate_clipboard_flags(flags, stream)?;
        Ok(Self {
            stream,
            pointer_enabled: flags & FLAG_POINTER != 0,
            keyboard_enabled: flags & FLAG_KEYBOARD != 0,
            clipboard_enabled: flags & FLAG_CLIPBOARD != 0,
            clipboard_files_enabled: flags & FLAG_CLIPBOARD_FILES != 0,
        })
    }
}

fn validate_clipboard_flags(flags: u8, stream: StreamCapabilities) -> RemoteResult<()> {
    if (stream.wayland_control
        || stream.wayland_text
        || stream.static_lossless
        || stream.wayland_popup_pointer)
        && flags & (INPUT_FLAGS | FLAG_CLIPBOARD | FLAG_CLIPBOARD_FILES) != 0
    {
        return Err(invalid_data("control cannot combine legacy input or clipboard flags").into());
    }
    if (flags & FLAG_CLIPBOARD != 0) != (stream.clipboard_content != 0)
        || (flags & FLAG_CLIPBOARD_FILES != 0) != (stream.clipboard_content & 4 != 0)
        || (stream.clipboard_file_limit != 0) != (stream.clipboard_content & 4 != 0)
    {
        return Err(invalid_data("clipboard flags, content and file limit disagree").into());
    }
    Ok(())
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

/// Source and geometry fences precede the unchanged atomic input codec.
pub const MAX_SOURCE_INPUT_PAYLOAD_LEN: usize = 16 + MAX_INPUT_BATCH_PAYLOAD_LEN;

pub fn encode_source_input(
    source_epoch: u64,
    geometry_epoch: u64,
    events: &[InputEvent],
) -> RemoteResult<Vec<u8>> {
    let mut output = Vec::new();
    encode_source_input_into(source_epoch, geometry_epoch, events, &mut output)?;
    Ok(output)
}

pub fn encode_source_input_into(
    source_epoch: u64,
    geometry_epoch: u64,
    events: &[InputEvent],
    output: &mut Vec<u8>,
) -> RemoteResult<()> {
    output.clear();
    if source_epoch == 0 || geometry_epoch == 0 {
        return Err(invalid_data("zero input source or geometry epoch").into());
    }
    encode_input_batch_into(events, output)?;
    let inner_len = output.len();
    output.resize(inner_len + 16, 0);
    output.copy_within(..inner_len, 16);
    output[..8].copy_from_slice(&source_epoch.to_be_bytes());
    output[8..16].copy_from_slice(&geometry_epoch.to_be_bytes());
    Ok(())
}

pub fn decode_source_input(payload: &[u8]) -> RemoteResult<(u64, u64, Vec<InputEvent>)> {
    if payload.len() < 17 || payload.len() > MAX_SOURCE_INPUT_PAYLOAD_LEN {
        return Err(invalid_data("invalid source input payload length").into());
    }
    let source_epoch = u64::from_be_bytes(payload[..8].try_into().unwrap());
    let geometry_epoch = u64::from_be_bytes(payload[8..16].try_into().unwrap());
    if source_epoch == 0 || geometry_epoch == 0 {
        return Err(invalid_data("zero input source or geometry epoch").into());
    }
    Ok((
        source_epoch,
        geometry_epoch,
        decode_input_batch(&payload[16..])?,
    ))
}

pub fn encode_source_frame_ack(source_epoch: u64, sequence: u64) -> RemoteResult<[u8; 16]> {
    if source_epoch == 0 {
        return Err(invalid_data("zero source epoch").into());
    }
    let mut payload = [0; 16];
    payload[..8].copy_from_slice(&source_epoch.to_be_bytes());
    payload[8..].copy_from_slice(&sequence.to_be_bytes());
    Ok(payload)
}

pub fn decode_source_frame_ack(payload: &[u8]) -> RemoteResult<(u64, u64)> {
    if payload.len() != 16 {
        return Err(invalid_data("invalid source frame acknowledgement length").into());
    }
    let epoch = u64::from_be_bytes(payload[..8].try_into().unwrap());
    let sequence = u64::from_be_bytes(payload[8..].try_into().unwrap());
    encode_source_frame_ack(epoch, sequence)?;
    Ok((epoch, sequence))
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
    #[test]
    fn horizontal_scroll_capability_is_independent_and_bounded() {
        use super::StreamCapabilities;
        for control in [false, true] {
            for (vertical, horizontal) in
                [(false, false), (true, false), (false, true), (true, true)]
            {
                let caps = StreamCapabilities {
                    wayland_pointer: !control,
                    wayland_control: control,
                    wayland_scroll: vertical,
                    wayland_horizontal_scroll: horizontal,
                    ..Default::default()
                };
                assert_eq!(StreamCapabilities::decode(&caps.encode()).unwrap(), caps);
                assert_eq!(
                    caps.encode()[28],
                    u8::from(vertical) | (u8::from(horizontal) << 1)
                );
            }
        }
        let invalid = StreamCapabilities {
            wayland_horizontal_scroll: true,
            ..Default::default()
        };
        assert!(StreamCapabilities::decode(&invalid.encode()).is_err());
    }
    #[test]
    fn wayland_keyboard_capability_is_independent_and_strict() {
        let stream = super::StreamCapabilities {
            wayland_keyboard: true,
            ..Default::default()
        };
        let mut bytes = stream.encode();
        assert_eq!(super::StreamCapabilities::decode(&bytes).unwrap(), stream);
        assert_eq!(bytes[25], 0, "keyboard must not enable resize");
        bytes[26] = 2;
        assert!(super::StreamCapabilities::decode(&bytes).is_err());
    }

    #[test]
    fn explicit_window_resize_capability_is_bounded() {
        let stream = super::StreamCapabilities {
            window_resize: true,
            ..Default::default()
        };
        let mut encoded = stream.encode();
        assert_eq!(super::StreamCapabilities::decode(&encoded).unwrap(), stream);
        encoded[25] = 2;
        assert!(super::StreamCapabilities::decode(&encoded).is_err());
        assert!(super::StreamCapabilities::decode(&encoded[..25]).is_err());
    }
    use super::*;

    #[test]
    fn source_input_and_ack_are_scoped_and_bounded() {
        let events = vec![InputEvent::Pointer { x: 1, y: 2 }; MAX_INPUT_BATCH_EVENTS];
        let payload = encode_source_input(4, 7, &events).unwrap();
        assert_eq!(payload.len(), MAX_SOURCE_INPUT_PAYLOAD_LEN);
        let (source, geometry, decoded) = decode_source_input(&payload).unwrap();
        assert_eq!((source, geometry), (4, 7));
        assert_eq!(decoded, events);
        for len in 0..payload.len() {
            assert!(decode_source_input(&payload[..len]).is_err());
        }
        let mut trailing = payload.clone();
        trailing.push(0);
        assert!(decode_source_input(&trailing).is_err());
        for offset in [0, 8] {
            let mut bad = payload.clone();
            bad[offset..offset + 8].fill(0);
            assert!(decode_source_input(&bad).is_err());
        }
        let mut output = vec![9];
        assert!(encode_source_input_into(0, 1, &events, &mut output).is_err());
        assert!(output.is_empty());
        assert!(encode_source_input_into(1, 1, &[], &mut output).is_err());
        assert!(output.is_empty());
        let ack = encode_source_frame_ack(4, 8).unwrap();
        assert_eq!(decode_source_frame_ack(&ack).unwrap(), (4, 8));
        for len in 0..16 {
            assert!(decode_source_frame_ack(&ack[..len]).is_err());
        }
        assert!(decode_source_frame_ack(&[0; 17]).is_err());
        assert!(encode_source_frame_ack(0, 8).is_err());
        assert_eq!(
            decode_source_frame_ack(&encode_source_frame_ack(4, 0).unwrap()).unwrap(),
            (4, 0)
        );
    }

    #[test]
    fn frame_limits_validate_intersect_and_reject_malformed_wire_values() {
        let viewer = FrameLimits {
            max_width: 1920,
            max_height: 1080,
            max_pixels: 1_000_000,
        };
        let host = FrameLimits {
            max_width: 1280,
            max_height: 2160,
            max_pixels: 2_000_000,
        };
        let selected = viewer.intersect(host);
        assert_eq!(
            selected,
            FrameLimits {
                max_width: 1280,
                max_height: 1080,
                max_pixels: 1_000_000
            }
        );
        assert!(selected.validate_dimensions(1000, 1000).is_ok());
        for dims in [
            (1001, 1000),
            (1281, 1),
            (1, 1081),
            (0, 1),
            (1, 0),
            (u16::MAX, u16::MAX),
        ] {
            assert!(selected.validate_dimensions(dims.0, dims.1).is_err());
        }
        for limits in [
            FrameLimits {
                max_width: 0,
                ..viewer
            },
            FrameLimits {
                max_height: 0,
                ..viewer
            },
            FrameLimits {
                max_pixels: 0,
                ..viewer
            },
            FrameLimits {
                max_width: u16::MAX,
                ..viewer
            },
            FrameLimits {
                max_height: u16::MAX,
                ..viewer
            },
            FrameLimits {
                max_pixels: u64::MAX,
                ..viewer
            },
        ] {
            assert!(limits.validate().is_err());
            let stream = StreamCapabilities {
                frame_limits: limits,
                ..Default::default()
            };
            assert!(StreamCapabilities::decode(&stream.encode()).is_err());
        }
        for limits in [
            viewer,
            selected,
            FrameLimits {
                max_width: 1,
                max_height: 1,
                max_pixels: 1,
            },
            FrameLimits::default(),
        ] {
            let stream = StreamCapabilities {
                frame_limits: limits,
                ..Default::default()
            };
            assert_eq!(
                StreamCapabilities::decode(&stream.encode()).unwrap(),
                stream
            );
        }
    }

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
            assert!(
                error
                    .to_string()
                    .contains(&format!("version 6; expected {APPLICATION_VERSION}"))
            );
        }
    }

    #[test]
    fn oversized_client_hello_reports_version_mismatch_before_length() {
        let mut payload = ClientHello {
            stream: Default::default(),
            initial_viewport: None,
            request_input: false,
            request_clipboard: false,
            request_clipboard_files: false,
            keymap: KeymapTable::empty(),
        }
        .encode();
        // Versions 15-17 each added one capability byte. With a full keymap,
        // their hellos exceed version 14's maximum even though the prefix fits.
        payload.resize(MAX_CLIENT_HELLO_LEN + 3, 0);
        for version in [APPLICATION_VERSION - 1, APPLICATION_VERSION + 1] {
            payload[8..10].copy_from_slice(&version.to_be_bytes());
            assert_eq!(
                ClientHello::decode(&payload).unwrap_err().to_string(),
                format!(
                    "unsupported JWM remote application version {version}; expected {APPLICATION_VERSION}"
                )
            );
        }
        // Moving the check must not admit oversized messages of our version.
        payload[8..10].copy_from_slice(&APPLICATION_VERSION.to_be_bytes());
        assert_eq!(
            ClientHello::decode(&payload).unwrap_err().to_string(),
            "client hello has an invalid length"
        );
        payload[0] ^= 1;
        assert_eq!(
            ClientHello::decode(&payload).unwrap_err().to_string(),
            "peer is not using the JWM remote protocol"
        );
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
                    stream: StreamCapabilities {
                        clipboard_content: if request_clipboard { 7 } else { 0 },
                        clipboard_directions: if request_clipboard { 3 } else { 0 },
                        clipboard_file_limit: if request_clipboard { 100 } else { 0 },
                        ..Default::default()
                    },
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
                    stream: StreamCapabilities {
                        clipboard_content: if clipboard { 3 } else { 0 },
                        clipboard_directions: if clipboard { 3 } else { 0 },
                        ..Default::default()
                    },
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
            stream: StreamCapabilities {
                clipboard_content: 3,
                clipboard_directions: 3,
                ..Default::default()
            },
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
    fn hello_uses_current_version_and_rejects_neighbouring_versions() {
        let payload = ServerHello {
            stream: Default::default(),
            pointer_enabled: false,
            keyboard_enabled: false,
            clipboard_enabled: false,
            clipboard_files_enabled: false,
        }
        .encode();
        assert_eq!(&payload[8..10], &APPLICATION_VERSION.to_be_bytes());

        // Version 5 had no clipboard transfer records: a version-5 peer
        // would end the session on the first copied image or file. It must
        // fail on its version instead. Version 4 sent one digest of the whole
        // keymap where later versions send a per-keycode table.
        for older in 4_u16..APPLICATION_VERSION {
            let mut previous = payload;
            previous[8..10].copy_from_slice(&older.to_be_bytes());
            let error = ServerHello::decode(&previous).unwrap_err();
            assert!(error.to_string().contains(&format!(
                "application version {older}; expected {APPLICATION_VERSION}"
            )));
        }

        for rejected in [0_u16, 2, 3, APPLICATION_VERSION + 1, u16::MAX] {
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
            stream: StreamCapabilities {
                clipboard_content: 7,
                clipboard_directions: 3,
                clipboard_file_limit: 100,
                ..Default::default()
            },
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
        assert!(ServerHello::decode(&files_only.encode()).is_err());
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
    fn clipboard_wire_policy_is_strict_and_round_trips() {
        for content in [0, 1, 3, 7] {
            for directions in 0..=3 {
                let hello = ServerHello {
                    stream: StreamCapabilities {
                        clipboard_content: content,
                        clipboard_directions: directions,
                        clipboard_file_limit: if content & 4 != 0 { 100 } else { 0 },
                        ..Default::default()
                    },
                    clipboard_enabled: content != 0,
                    clipboard_files_enabled: content & 4 != 0,
                    pointer_enabled: false,
                    keyboard_enabled: false,
                };
                assert_eq!(
                    ServerHello::decode(&hello.encode()).is_ok(),
                    (content == 0) == (directions == 0)
                );
            }
        }
        let hello = ServerHello {
            stream: StreamCapabilities {
                clipboard_content: 7,
                clipboard_directions: 3,
                clipboard_file_limit: 100,
                ..Default::default()
            },
            clipboard_enabled: true,
            clipboard_files_enabled: true,
            pointer_enabled: false,
            keyboard_enabled: false,
        };
        for content in [2, 4, 5, 6, 8, 255] {
            let mut bytes = hello.encode();
            bytes[HELLO_PREFIX_LEN + 23] = content;
            assert!(ServerHello::decode(&bytes).is_err());
        }
        for direction in [4, 128, 255] {
            let mut bytes = hello.encode();
            bytes[HELLO_PREFIX_LEN + 24] = direction;
            assert!(ServerHello::decode(&bytes).is_err());
        }
        for index in [10, HELLO_PREFIX_LEN + 23, HELLO_PREFIX_LEN + 24] {
            let mut bytes = hello.encode();
            bytes[index] = 0;
            assert!(ServerHello::decode(&bytes).is_err());
        }
        let mut bytes = hello.encode();
        bytes[HELLO_PREFIX_LEN + 3..HELLO_PREFIX_LEN + 11].fill(0);
        assert!(ServerHello::decode(&bytes).is_err());
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

#[cfg(test)]
mod scroll_capability_tests {
    use super::*;
    #[test]
    fn scroll_is_explicit_boolean_and_requires_pointer_or_control_in_version_23() {
        assert_eq!(APPLICATION_VERSION, 31);
        assert_eq!(STREAM_CAPABILITIES_LEN, 34);
        let mut caps = StreamCapabilities::default();
        assert!(
            !StreamCapabilities::decode(&caps.encode())
                .unwrap()
                .wayland_scroll
        );
        caps.wayland_scroll = true;
        assert!(StreamCapabilities::decode(&caps.encode()).is_err());
        caps.wayland_pointer = true;
        assert_eq!(StreamCapabilities::decode(&caps.encode()).unwrap(), caps);
        for flag in [4, 128, 255] {
            let mut payload = caps.encode();
            payload[28] = flag;
            assert!(StreamCapabilities::decode(&payload).is_err());
        }
        assert!(StreamCapabilities::decode(&caps.encode()[..28]).is_err());
    }
}

#[cfg(test)]
mod scroll_copy_capability_tests {
    use super::*;
    #[test]
    fn scroll_copy_is_bounded_video_only_capability() {
        let mut caps = StreamCapabilities::default();
        assert!(!caps.scroll_copy);
        caps.scroll_copy = true;
        assert_eq!(StreamCapabilities::decode(&caps.encode()).unwrap(), caps);
        assert!(!caps.wayland_pointer && !caps.wayland_scroll && !caps.wayland_keyboard);
        for host in [false, true] {
            for peer in [false, true] {
                caps.scroll_copy = peer;
                assert_eq!(caps.negotiate_scroll_copy(host), host && peer);
            }
        }
        caps.scroll_copy = true;
        for invalid in [2, 255] {
            let mut payload = caps.encode();
            payload[29] = invalid;
            assert!(StreamCapabilities::decode(&payload).is_err());
        }
        assert!(StreamCapabilities::decode(&caps.encode()[..29]).is_err());
    }
}

#[cfg(test)]
mod control_capability_tests {
    use super::*;
    #[test]
    fn control_is_explicit_bounded_boolean_and_excludes_old_permissions() {
        let caps = StreamCapabilities {
            wayland_control: true,
            ..Default::default()
        };
        let bytes = caps.encode();
        assert_eq!(bytes.len(), 34);
        assert_eq!(StreamCapabilities::decode(&bytes).unwrap(), caps);
        for v in [2, 255] {
            let mut bad = bytes;
            bad[30] = v;
            assert!(StreamCapabilities::decode(&bad).is_err());
        }
        let mut scroll = caps;
        scroll.wayland_scroll = true;
        assert_eq!(
            StreamCapabilities::decode(&scroll.encode()).unwrap(),
            scroll
        );
        scroll.wayland_keyboard = true;
        assert!(StreamCapabilities::decode(&scroll.encode()).is_err());
        for index in [23, 24, 25, 26, 27] {
            let mut bad = bytes;
            bad[index] = 1;
            assert!(StreamCapabilities::decode(&bad).is_err());
        }
        assert!(StreamCapabilities::decode(&bytes[..30]).is_err());
        assert!(!StreamCapabilities::default().wayland_control);
        for flags in [
            FLAG_POINTER,
            FLAG_KEYBOARD,
            INPUT_FLAGS,
            FLAG_CLIPBOARD,
            FLAG_CLIPBOARD_FILES,
        ] {
            let hello = encode_hello(flags, caps);
            assert!(ServerHello::decode(&hello).is_err());
        }
    }
}

#[cfg(test)]
mod text_capability_tests {
    use super::*;
    #[test]
    fn text_is_separate_default_off_and_exclusive() {
        assert!(!StreamCapabilities::default().wayland_text);
        let c = StreamCapabilities {
            wayland_text: true,
            ..Default::default()
        };
        let bytes = c.encode();
        assert_eq!(bytes.len(), 34);
        assert_eq!(StreamCapabilities::decode(&bytes).unwrap(), c);
        for i in [23, 24, 25, 26, 27, 28, 30] {
            let mut b = bytes;
            b[i] = 1;
            assert!(StreamCapabilities::decode(&b).is_err(), "{i}");
        }
        for v in [2, 255] {
            let mut b = bytes;
            b[31] = v;
            assert!(StreamCapabilities::decode(&b).is_err());
        }
    }
}

#[cfg(test)]
mod static_lossless_capability_tests {
    use super::*;
    #[test]
    fn explicit_readonly_and_bounded_boolean() {
        assert!(!StreamCapabilities::default().static_lossless);
        let c = StreamCapabilities {
            static_lossless: true,
            ..Default::default()
        };
        let bytes = c.encode();
        assert_eq!(StreamCapabilities::decode(&bytes).unwrap(), c);
        for i in [23, 24, 25, 28, 31] {
            let mut b = bytes;
            b[i] = 1;
            assert!(StreamCapabilities::decode(&b).is_err(), "{i}");
        }
        let mut control = bytes;
        control[30] = 1;
        assert!(
            StreamCapabilities::decode(&control)
                .unwrap()
                .wayland_control
        );
        let mut keyboard = bytes;
        keyboard[26] = 1;
        assert!(
            StreamCapabilities::decode(&keyboard)
                .unwrap()
                .wayland_keyboard
        );
        let mut pointer = bytes;
        pointer[27] = 1;
        assert!(
            StreamCapabilities::decode(&pointer)
                .unwrap()
                .wayland_pointer
        );
        // Each scroll axis is independently allowed with native pointer only.
        // Every other input/clipboard/resize combination remains rejected.
        let mut vertical = pointer;
        vertical[28] = 1;
        assert!(
            StreamCapabilities::decode(&vertical)
                .unwrap()
                .wayland_scroll
        );
        for i in [23, 24, 25, 26, 30, 31] {
            let mut mixed = pointer;
            mixed[i] = 1;
            assert!(StreamCapabilities::decode(&mixed).is_err(), "mixed {i}");
        }
        for v in [2, 3] {
            let mut scroll = pointer;
            scroll[28] = v;
            let decoded = StreamCapabilities::decode(&scroll).unwrap();
            assert!(decoded.wayland_horizontal_scroll);
            assert_eq!(decoded.wayland_scroll, v == 3);
        }
        for v in [2, 255] {
            let mut b = bytes;
            b[32] = v;
            assert!(StreamCapabilities::decode(&b).is_err());
        }
        for flags in [
            FLAG_POINTER,
            FLAG_KEYBOARD,
            FLAG_CLIPBOARD,
            FLAG_CLIPBOARD_FILES,
        ] {
            assert!(ServerHello::decode(&encode_hello(flags, c)).is_err());
        }
    }
}

#[cfg(test)]
mod lossless_scroll_hello_tests {
    use super::*;

    fn client(stream: StreamCapabilities) -> ClientHello {
        ClientHello {
            stream,
            initial_viewport: None,
            request_input: false,
            request_clipboard: false,
            request_clipboard_files: false,
            keymap: KeymapTable::empty(),
        }
    }
    fn server(stream: StreamCapabilities) -> ServerHello {
        ServerHello {
            stream,
            pointer_enabled: false,
            keyboard_enabled: false,
            clipboard_enabled: false,
            clipboard_files_enabled: false,
        }
    }

    #[test]
    fn lossless_vertical_pointer_roundtrips_both_real_hellos() {
        let caps = StreamCapabilities {
            static_lossless: true,
            wayland_pointer: true,
            wayland_scroll: true,
            ..Default::default()
        };
        let c = client(caps);
        let s = server(caps);
        assert_eq!(ClientHello::decode(&c.encode()).unwrap(), c);
        assert_eq!(ServerHello::decode(&s.encode()).unwrap(), s);
        for version in [27_u16, 28, 29, 30, 32] {
            let mut cb = c.encode();
            cb[8..10].copy_from_slice(&version.to_be_bytes());
            assert!(ClientHello::decode(&cb).is_err());
            let mut sb = s.encode();
            sb[8..10].copy_from_slice(&version.to_be_bytes());
            assert!(ServerHello::decode(&sb).is_err());
        }
    }

    #[test]
    fn horizontal_only_and_both_axes_roundtrip_without_implicit_vertical_permission() {
        for vertical in [false, true] {
            let caps = StreamCapabilities {
                static_lossless: true,
                wayland_pointer: true,
                wayland_scroll: vertical,
                wayland_horizontal_scroll: true,
                ..Default::default()
            };
            assert_eq!(StreamCapabilities::decode(&caps.encode()).unwrap(), caps);
            assert_eq!(
                ClientHello::decode(&client(caps).encode()).unwrap(),
                client(caps)
            );
            assert_eq!(
                ServerHello::decode(&server(caps).encode()).unwrap(),
                server(caps)
            );
        }
    }

    #[test]
    fn lossless_input_matrix_is_identical_in_both_hello_decoders() {
        // Exhaustively vary native mode and axis bits with PNG enabled.
        for mask in 0_u8..64 {
            let caps = StreamCapabilities {
                static_lossless: true,
                wayland_keyboard: mask & 1 != 0,
                wayland_pointer: mask & 2 != 0,
                wayland_control: mask & 4 != 0,
                wayland_text: mask & 8 != 0,
                wayland_scroll: mask & 16 != 0,
                wayland_horizontal_scroll: mask & 32 != 0,
                ..Default::default()
            };
            let permitted = !(caps.wayland_control
                && (caps.wayland_keyboard
                    || caps.wayland_pointer
                    || caps.wayland_scroll
                    || caps.wayland_horizontal_scroll))
                && !caps.wayland_text
                && !(caps.wayland_keyboard && caps.wayland_pointer)
                && (!(caps.wayland_scroll || caps.wayland_horizontal_scroll)
                    || (caps.wayland_pointer && !caps.wayland_keyboard));
            assert_eq!(
                StreamCapabilities::decode(&caps.encode()).is_ok(),
                permitted,
                "caps mask {mask}"
            );
            assert_eq!(
                ClientHello::decode(&client(caps).encode()).is_ok(),
                permitted,
                "client mask {mask}"
            );
            assert_eq!(
                ServerHello::decode(&server(caps).encode()).is_ok(),
                permitted,
                "server mask {mask}"
            );
        }
    }

    #[test]
    fn lossless_vertical_pointer_rejects_legacy_clipboard_resize_and_unknown_bits() {
        let caps = StreamCapabilities {
            static_lossless: true,
            wayland_pointer: true,
            wayland_scroll: true,
            ..Default::default()
        };
        for kind in 0..4 {
            let mut c = client(caps);
            let mut s = server(caps);
            match kind {
                0 => {
                    c.request_input = true;
                    s.pointer_enabled = true;
                }
                1 => {
                    c.request_clipboard = true;
                    s.clipboard_enabled = true;
                }
                2 => {
                    c.request_clipboard = true;
                    s.clipboard_enabled = true;
                    c.request_clipboard_files = true;
                    s.clipboard_files_enabled = true;
                }
                _ => {
                    c.stream.window_resize = true;
                    s.stream.window_resize = true;
                }
            }
            assert!(ClientHello::decode(&c.encode()).is_err());
            assert!(ServerHello::decode(&s.encode()).is_err());
        }
        for bits in [4, 5, 6, 7, 128, 255] {
            let mut cb = client(caps).encode();
            cb[HELLO_PREFIX_LEN + 28] = bits;
            assert!(ClientHello::decode(&cb).is_err());
            let mut sb = server(caps).encode();
            sb[HELLO_PREFIX_LEN + 28] = bits;
            assert!(ServerHello::decode(&sb).is_err());
        }
    }
}

#[cfg(test)]
mod popup_capability_tests {
    use super::*;
    fn client(stream: StreamCapabilities) -> ClientHello {
        ClientHello {
            stream,
            initial_viewport: None,
            request_input: false,
            request_clipboard: false,
            request_clipboard_files: false,
            keymap: KeymapTable::empty(),
        }
    }
    fn server(stream: StreamCapabilities) -> ServerHello {
        ServerHello {
            stream,
            pointer_enabled: false,
            keyboard_enabled: false,
            clipboard_enabled: false,
            clipboard_files_enabled: false,
        }
    }
    #[test]
    fn popup_actual_hellos_are_default_off_exclusive_and_strict() {
        assert!(!StreamCapabilities::default().wayland_popup_pointer);
        let caps = StreamCapabilities {
            wayland_popup_pointer: true,
            ..Default::default()
        };
        assert_eq!(StreamCapabilities::decode(&caps.encode()).unwrap(), caps);
        assert_eq!(
            ClientHello::decode(&client(caps).encode()).unwrap(),
            client(caps)
        );
        assert_eq!(
            ServerHello::decode(&server(caps).encode()).unwrap(),
            server(caps)
        );
        for index in 23..33 {
            for value in [1, 2, 3, 255] {
                let mut c = client(caps).encode();
                c[HELLO_PREFIX_LEN + index] = value;
                let mut s = server(caps).encode();
                s[HELLO_PREFIX_LEN + index] = value;
                assert!(ClientHello::decode(&c).is_err(), "client {index}/{value}");
                assert!(ServerHello::decode(&s).is_err(), "server {index}/{value}");
            }
        }
        for value in [2, 3, 128, 255] {
            let mut c = client(caps).encode();
            c[HELLO_PREFIX_LEN + 33] = value;
            let mut s = server(caps).encode();
            s[HELLO_PREFIX_LEN + 33] = value;
            assert!(ClientHello::decode(&c).is_err());
            assert!(ServerHello::decode(&s).is_err());
        }
        for flag in [
            FLAG_POINTER,
            FLAG_KEYBOARD,
            FLAG_CLIPBOARD,
            FLAG_CLIPBOARD_FILES,
            128,
        ] {
            let mut c = client(caps).encode();
            c[10] |= flag;
            let mut s = server(caps).encode();
            s[10] |= flag;
            assert!(ClientHello::decode(&c).is_err());
            assert!(ServerHello::decode(&s).is_err());
        }
        for version in [0_u16, 29, 30, 32, u16::MAX] {
            let mut c = client(caps).encode();
            c[8..10].copy_from_slice(&version.to_be_bytes());
            let mut s = server(caps).encode();
            s[8..10].copy_from_slice(&version.to_be_bytes());
            assert!(ClientHello::decode(&c).is_err());
            assert!(ServerHello::decode(&s).is_err());
        }
        let b = caps.encode();
        assert!(StreamCapabilities::decode(&b[..33]).is_err());
        let mut b = b.to_vec();
        b.push(0);
        assert!(StreamCapabilities::decode(&b).is_err());
    }
}
