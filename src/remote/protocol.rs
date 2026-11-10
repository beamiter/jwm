//! Authenticated and encrypted wire protocol for JWM's remote-control transport.
//!
//! Transport v3 uses standard Noise NNpsk2 (25519/ChaChaPoly/SHA256), then
//! exchanges an encrypted random directional-key envelope and both Finished
//! confirmations. Public role-ordered headers/nonces are bound by the prologue.
//! No early application data, negotiation fallback, or raw Noise key export.
//!
//! After the handshake, records have the following layout:
//!
//! ```text
//! kind (u8) || sequence (u64, BE) || ciphertext_len (u32, BE)
//!     || ciphertext || Poly1305 tag (16 bytes)
//! ```
//!
//! The plaintext is `payload || 0x01 || 0x00 * padding`, sealed with
//! ChaCha20-Poly1305 under a nonce of four zero bytes followed by the record's
//! big-endian sequence number. The 13-byte header is authenticated as
//! associated data, so the kind, the sequence and the length are all covered
//! without being hidden from the length checks that bound allocation. Sequence
//! numbers start at zero, increase by exactly one, and exhaust as a hard error,
//! so a nonce is never reused under a key.
//!
//! Small records are padded to a size bucket. Length alone otherwise
//! distinguishes a keystroke batch from a pointer run from a release-all.
//! Inter-keystroke *timing* remains a side channel; padding does not hide it.
//!
//! Callers must close the connection after any [`ProtocolError`], and should
//! set handshake/read/write deadlines on their underlying stream.

use std::fmt;
use std::io::{self, Read, Write};

use chacha20poly1305::aead::AeadInOut;
use chacha20poly1305::{ChaCha20Poly1305, Key, KeyInit, Nonce, Tag};
use zeroize::{Zeroize, Zeroizing};

/// Required size of the pre-shared key.
pub const PSK_LEN: usize = 32;

/// Size of each handshake nonce.
pub const NONCE_LEN: usize = 32;

/// Size of each directional record key.
pub const MAC_LEN: usize = 32;

/// Size of the Poly1305 authentication tag on every record.
pub const TAG_LEN: usize = 16;

/// Transport magic, so a non-jwm-remote peer fails immediately and loudly.
const TRANSPORT_MAGIC: &[u8; 5] = b"JWMRT";

/// Transport version. Version 1 was authenticated-but-unencrypted; version 2
/// encrypts every record; version 3 adds a forward-secret Noise handshake.
pub const TRANSPORT_VERSION: u16 = 3;

const TRANSPORT_PREFIX_LEN: usize = TRANSPORT_MAGIC.len() + 2;

/// Maximum accepted payload size for one authenticated record (32 MiB).
pub const MAX_PAYLOAD_LEN: usize = 32 * 1024 * 1024;

const RECORD_HEADER_LEN: usize = 1 + 8 + 4;
/// Marks the end of the real payload inside a padded plaintext.
const PADDING_MARKER: u8 = 0x01;
/// Buckets small records so their length stops identifying their contents.
/// Anything larger is dominated by screen content and is not worth padding.
const PADDING_BUCKETS: [usize; 3] = [64, 256, 704];
const RETAINED_PAYLOAD_SOFT_LIMIT: usize = 8 * 1024 * 1024;
const UNDERUSED_PAYLOADS_BEFORE_SHRINK: u8 = 32;

/// Applies conservative hysteresis to long-lived reusable record buffers.
///
/// Stable frame sizes retain their allocation indefinitely. After an extreme
/// frame, thirty-two consecutive payloads using at most a quarter of that
/// capacity allow the buffer to shrink back toward an 8 MiB soft ceiling.
#[derive(Default)]
pub(crate) struct PayloadBufferRetention {
    underused_payloads: u8,
}

impl PayloadBufferRetention {
    pub(crate) fn observe(&mut self, payload: &mut Vec<u8>) {
        let capacity = payload.capacity();
        if !self.should_shrink(payload.len(), capacity) {
            return;
        }
        payload.shrink_to(payload.len().max(RETAINED_PAYLOAD_SOFT_LIMIT));
    }

    fn should_shrink(&mut self, len: usize, capacity: usize) -> bool {
        if capacity <= RETAINED_PAYLOAD_SOFT_LIMIT || len > capacity / 4 {
            self.underused_payloads = 0;
            return false;
        }
        self.underused_payloads = self.underused_payloads.saturating_add(1);
        if self.underused_payloads < UNDERUSED_PAYLOADS_BEFORE_SHRINK {
            return false;
        }
        self.underused_payloads = 0;
        true
    }
}

/// Application-level message type carried by an authenticated record.
///
/// Payload encoding is defined by the host/client message layer. Keeping the
/// transport kinds small and explicit prevents the local JWM IPC command set
/// from accidentally becoming a network API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MessageKind {
    /// Client capability negotiation after transport authentication.
    Hello = 1,
    /// Server capability acknowledgement.
    HelloAck = 2,
    /// One complete encoded frame.
    Frame = 3,
    /// Legacy single pointer payload; version-3 peers use [`Self::InputBatch`].
    Pointer = 4,
    /// Legacy single key payload; version-3 peers use [`Self::InputBatch`].
    Key = 5,
    /// Legacy single button payload; version-3 peers use [`Self::InputBatch`].
    Button = 6,
    /// Legacy single release-all payload; version-3 peers use [`Self::InputBatch`].
    ReleaseAll = 7,
    /// Graceful session shutdown.
    Close = 8,
    /// Client activity heartbeat; video frames provide the reverse heartbeat.
    Heartbeat = 9,
    /// Cumulative acknowledgement of the latest frame drawn by the client.
    FrameAck = 10,
    /// One atomically decoded batch of pointer, key, button, or release events.
    InputBatch = 11,
    /// Client's current viewable size, so the host stops encoding pixels the
    /// viewer cannot display.
    Viewport = 12,
    /// UTF-8 clipboard text. Travels in both directions.
    Clipboard = 13,
    /// One step of a chunked clipboard image or file transfer, or its flow
    /// control. Travels in both directions; see `clipboard_sync`.
    ClipboardTransfer = 14,
    /// Bounded independent cursor shape (server to viewer).
    CursorShape = 15,
    /// Latest cursor position tied to a video geometry epoch.
    CursorPosition = 16,
    /// Immutable host-approved source choices.
    SourceCatalog = 17,
    /// Request to select an already granted source.
    SourceRequest = 18,
    /// Committed source identity and new epoch.
    SourceChanged = 19,
    /// Structured host termination, never a reconnect or cleanup guarantee.
    SessionClose = 20,
    /// Viewer requests a monotonic clipboard content restriction.
    ClipboardRestrict = 21,
    /// Host worker, backend and ordered writer have crossed the restriction.
    ClipboardRestricted = 22,
    WindowResizeRequest = 23,
    WindowResizeResult = 24,
    WaylandKeyboardRequest = 25,
    WaylandKeyboardStatus = 26,
    WaylandKeyboardKey = 27,
    WaylandKeyboardRevoke = 28,
    WaylandPointerRequest = 29,
    WaylandPointerStatus = 30,
    WaylandPointerAction = 31,
    WaylandPointerRevoke = 32,
    WaylandControlRequest = 33,
    WaylandControlStatus = 34,
    WaylandControlAction = 35,
    WaylandControlRevoke = 36,
    WaylandTextCommit = 37,
    WaylandTextResult = 38,
}

impl TryFrom<u8> for MessageKind {
    type Error = ProtocolError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Hello),
            2 => Ok(Self::HelloAck),
            3 => Ok(Self::Frame),
            4 => Ok(Self::Pointer),
            5 => Ok(Self::Key),
            6 => Ok(Self::Button),
            7 => Ok(Self::ReleaseAll),
            8 => Ok(Self::Close),
            9 => Ok(Self::Heartbeat),
            10 => Ok(Self::FrameAck),
            11 => Ok(Self::InputBatch),
            12 => Ok(Self::Viewport),
            13 => Ok(Self::Clipboard),
            14 => Ok(Self::ClipboardTransfer),
            15 => Ok(Self::CursorShape),
            16 => Ok(Self::CursorPosition),
            17 => Ok(Self::SourceCatalog),
            18 => Ok(Self::SourceRequest),
            19 => Ok(Self::SourceChanged),
            20 => Ok(Self::SessionClose),
            23 => Ok(Self::WindowResizeRequest),
            24 => Ok(Self::WindowResizeResult),
            25 => Ok(Self::WaylandKeyboardRequest),
            26 => Ok(Self::WaylandKeyboardStatus),
            27 => Ok(Self::WaylandKeyboardKey),
            28 => Ok(Self::WaylandKeyboardRevoke),
            29 => Ok(Self::WaylandPointerRequest),
            30 => Ok(Self::WaylandPointerStatus),
            31 => Ok(Self::WaylandPointerAction),
            32 => Ok(Self::WaylandPointerRevoke),
            33 => Ok(Self::WaylandControlRequest),
            34 => Ok(Self::WaylandControlStatus),
            35 => Ok(Self::WaylandControlAction),
            36 => Ok(Self::WaylandControlRevoke),
            37 => Ok(Self::WaylandTextCommit),
            38 => Ok(Self::WaylandTextResult),
            21 => Ok(Self::ClipboardRestrict),
            22 => Ok(Self::ClipboardRestricted),
            other => Err(ProtocolError::UnknownMessageKind(other)),
        }
    }
}

impl From<MessageKind> for u8 {
    fn from(value: MessageKind) -> Self {
        value as u8
    }
}

/// Errors produced while authenticating or exchanging remote records.
#[derive(Debug)]
pub enum ProtocolError {
    Io(io::Error),
    InvalidPskLength { expected: usize, actual: usize },
    AuthenticationFailed,
    UnknownMessageKind(u8),
    PayloadTooLarge { len: usize, max: usize },
    SequenceMismatch { expected: u64, received: u64 },
    SequenceExhausted,
    MalformedPadding,
    UnsupportedTransportVersion { received: u16, expected: u16 },
    BadTransportMagic,
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "remote protocol I/O error: {error}"),
            Self::InvalidPskLength { expected, actual } => write!(
                f,
                "invalid remote PSK length: expected {expected} bytes, got {actual}"
            ),
            Self::AuthenticationFailed => f.write_str("remote peer authentication failed"),
            Self::UnknownMessageKind(kind) => {
                write!(f, "unknown remote message kind {kind}")
            }
            Self::PayloadTooLarge { len, max } => {
                write!(f, "remote payload is {len} bytes; maximum is {max}")
            }
            Self::SequenceMismatch { expected, received } => write!(
                f,
                "remote record sequence mismatch: expected {expected}, received {received}"
            ),
            Self::SequenceExhausted => f.write_str("remote record sequence exhausted"),
            Self::MalformedPadding => f.write_str("remote record padding is malformed"),
            Self::UnsupportedTransportVersion { received, expected } => write!(
                f,
                "remote transport version {received}; expected {expected}. Update jwm-remote on both machines together"
            ),
            Self::BadTransportMagic => {
                f.write_str("peer did not present the jwm-remote transport magic")
            }
        }
    }
}

impl std::error::Error for ProtocolError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for ProtocolError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

/// Directional record keys produced by a successful handshake.
///
/// The fields are public so a caller can clone its connected `TcpStream` and
/// bind the appropriate key to each half. Prefer [`SessionKeys::into_client`]
/// and [`SessionKeys::into_server`] to avoid accidentally swapping them.
pub struct SessionKeys {
    pub c2s: [u8; MAC_LEN],
    pub s2c: [u8; MAC_LEN],
}

// Owned buffers are wiped on drop, but Rust copies and Snow internals are not
// guaranteed erased. Callers still own any copied array returned by this API.
impl Drop for SessionKeys {
    fn drop(&mut self) {
        self.c2s.zeroize();
        self.s2c.zeroize();
    }
}

impl SessionKeys {
    /// Return `(receive_key, send_key)` for the connecting client.
    pub fn into_client(self) -> ([u8; MAC_LEN], [u8; MAC_LEN]) {
        (self.s2c, self.c2s)
    }

    /// Return `(receive_key, send_key)` for the accepting server.
    pub fn into_server(self) -> ([u8; MAC_LEN], [u8; MAC_LEN]) {
        (self.c2s, self.s2c)
    }
}

/// Authenticated reader for one direction of a remote session.
pub struct SessionReader<R> {
    inner: R,
    key: Zeroizing<[u8; MAC_LEN]>,
    expected_sequence: u64,
    max_payload_len: usize,
}

impl<R> SessionReader<R> {
    /// Bind a reader to the receive key selected from [`SessionKeys`].
    pub fn new(inner: R, key: [u8; MAC_LEN]) -> Self {
        Self {
            inner,
            key: Zeroizing::new(key),
            expected_sequence: 0,
            max_payload_len: MAX_PAYLOAD_LEN,
        }
    }

    /// Lower the accepted record size for this direction.
    ///
    /// The global limit has to accommodate a whole screen frame, but most
    /// directions never legitimately carry one: the host receives only a
    /// hello, heartbeats, acknowledgements and input batches. Declaring the
    /// real ceiling keeps a peer from making the host reserve megabytes on the
    /// strength of an unauthenticated length field. Values above the global
    /// limit are ignored.
    pub fn set_max_payload_len(&mut self, max_payload_len: usize) {
        self.max_payload_len = max_payload_len.min(MAX_PAYLOAD_LEN);
    }

    /// Return the sequence number required for the next record.
    pub fn expected_sequence(&self) -> u64 {
        self.expected_sequence
    }

    /// Borrow the underlying reader.
    pub fn get_ref(&self) -> &R {
        &self.inner
    }

    /// Mutably borrow the underlying reader.
    pub fn get_mut(&mut self) -> &mut R {
        &mut self.inner
    }

    /// Consume the session wrapper and return the underlying reader.
    pub fn into_inner(self) -> R {
        self.inner
    }
}

impl<R: Read> SessionReader<R> {
    /// Read and authenticate the next record into a reusable payload buffer.
    ///
    /// The declared payload length is validated before resizing `payload`.
    /// Every error leaves its visible length at zero, so unauthenticated or
    /// truncated bytes are never returned to the caller. As with every
    /// protocol error, callers must close the connection rather than retry.
    pub fn read_message_into(
        &mut self,
        payload: &mut Vec<u8>,
    ) -> Result<MessageKind, ProtocolError> {
        let result = (|| {
            if self.expected_sequence == u64::MAX {
                return Err(ProtocolError::SequenceExhausted);
            }

            let mut header = [0_u8; RECORD_HEADER_LEN];
            self.inner.read_exact(&mut header)?;

            let received_sequence = u64::from_be_bytes(
                header[1..9]
                    .try_into()
                    .expect("record sequence field has a fixed length"),
            );
            if received_sequence != self.expected_sequence {
                return Err(ProtocolError::SequenceMismatch {
                    expected: self.expected_sequence,
                    received: received_sequence,
                });
            }

            let payload_len = u32::from_be_bytes(
                header[9..13]
                    .try_into()
                    .expect("record payload length field has a fixed length"),
            ) as usize;
            if payload_len > self.max_payload_len {
                return Err(ProtocolError::PayloadTooLarge {
                    len: payload_len,
                    max: self.max_payload_len,
                });
            }

            // The declared length is still plaintext, so allocation stays
            // bounded before a single byte of ciphertext is read.
            payload.resize(payload_len, 0);
            self.inner.read_exact(payload)?;

            let mut tag = [0_u8; TAG_LEN];
            self.inner.read_exact(&mut tag)?;
            cipher(&self.key)
                .decrypt_inout_detached(
                    &record_nonce(received_sequence),
                    &header,
                    payload.as_mut_slice().into(),
                    &Tag::from(tag),
                )
                .map_err(|_| ProtocolError::AuthenticationFailed)?;
            strip_padding(payload)?;

            // Message kind is authenticated as associated data. Do not let an
            // untrusted kind byte short-circuit consumption of the record.
            let kind = MessageKind::try_from(header[0])?;
            self.expected_sequence += 1;
            Ok(kind)
        })();
        if result.is_err() {
            payload.clear();
            if payload.capacity() > RETAINED_PAYLOAD_SOFT_LIMIT {
                *payload = Vec::new();
            }
        }
        result
    }

    /// Read and authenticate the next record.
    ///
    /// The declared payload length is checked before allocating its buffer.
    /// Sequence numbers must begin at zero and increase by exactly one.
    pub fn read_message(&mut self) -> Result<(MessageKind, Vec<u8>), ProtocolError> {
        let mut payload = Vec::new();
        let kind = self.read_message_into(&mut payload)?;
        Ok((kind, payload))
    }
}

/// Authenticated writer for one direction of a remote session.
pub struct SessionWriter<W> {
    inner: W,
    key: Zeroizing<[u8; MAC_LEN]>,
    next_sequence: u64,
    /// Reusable `header || ciphertext || tag` staging buffer.
    ///
    /// Sealing needs a mutable copy of the payload anyway, and assembling the
    /// whole record here turns three unbuffered socket writes per record into
    /// one -- both peers set `TCP_NODELAY` and neither wraps a `BufWriter`.
    record: Vec<u8>,
    retention: PayloadBufferRetention,
}

impl<W> SessionWriter<W> {
    /// Bind a writer to the send key selected from [`SessionKeys`].
    pub fn new(inner: W, key: [u8; MAC_LEN]) -> Self {
        Self {
            inner,
            key: Zeroizing::new(key),
            next_sequence: 0,
            record: Vec::new(),
            retention: PayloadBufferRetention::default(),
        }
    }

    /// Return the sequence number that will be used for the next record.
    pub fn next_sequence(&self) -> u64 {
        self.next_sequence
    }

    /// Borrow the underlying writer.
    pub fn get_ref(&self) -> &W {
        &self.inner
    }

    /// Mutably borrow the underlying writer.
    pub fn get_mut(&mut self) -> &mut W {
        &mut self.inner
    }

    /// Consume the session wrapper and return the underlying writer.
    pub fn into_inner(self) -> W {
        self.inner
    }
}

impl<W: Write> SessionWriter<W> {
    /// Authenticate and write one record.
    ///
    /// This method does not flush the writer; callers may batch records and use
    /// [`SessionWriter::flush`] at their preferred transport boundary.
    pub fn write_message(
        &mut self,
        kind: MessageKind,
        payload: &[u8],
    ) -> Result<(), ProtocolError> {
        if payload.len() > MAX_PAYLOAD_LEN {
            return Err(ProtocolError::PayloadTooLarge {
                len: payload.len(),
                max: MAX_PAYLOAD_LEN,
            });
        }
        if self.next_sequence == u64::MAX {
            return Err(ProtocolError::SequenceExhausted);
        }

        let sealed_len = padded_len(payload.len());
        if sealed_len > MAX_PAYLOAD_LEN {
            return Err(ProtocolError::PayloadTooLarge {
                len: sealed_len,
                max: MAX_PAYLOAD_LEN,
            });
        }

        let mut header = [0_u8; RECORD_HEADER_LEN];
        header[0] = kind.into();
        header[1..9].copy_from_slice(&self.next_sequence.to_be_bytes());
        header[9..13].copy_from_slice(&(sealed_len as u32).to_be_bytes());

        self.record.clear();
        self.record.extend_from_slice(&header);
        self.record.extend_from_slice(payload);
        self.record.push(PADDING_MARKER);
        self.record.resize(RECORD_HEADER_LEN + sealed_len, 0);

        let tag = cipher(&self.key)
            .encrypt_inout_detached(
                &record_nonce(self.next_sequence),
                &header,
                (&mut self.record[RECORD_HEADER_LEN..]).into(),
            )
            .map_err(|_| ProtocolError::AuthenticationFailed)?;
        self.record.extend_from_slice(&tag);

        self.inner.write_all(&self.record)?;
        self.next_sequence += 1;
        let mut record = std::mem::take(&mut self.record);
        self.retention.observe(&mut record);
        record.clear();
        self.record = record;
        Ok(())
    }

    /// Flush the underlying writer.
    pub fn flush(&mut self) -> Result<(), ProtocolError> {
        self.inner.flush()?;
        Ok(())
    }
}

#[path = "protocol_handshake.rs"]
mod handshake;

/// Authenticate with Noise and both Finished confirmations. `client_nonce` is
/// public context only; fresh ephemeral secrets come from Snow's OS RNG.
/// The caller must retain one absolute deadline over the complete operation.
pub fn client_handshake<S: Read + Write>(
    stream: &mut S,
    psk: &[u8],
    client_nonce: [u8; NONCE_LEN],
) -> Result<SessionKeys, ProtocolError> {
    handshake::client(stream, psk, client_nonce)
}
/// Server counterpart; no session keys escape before client Finished verifies
/// and server Finished has been written and flushed. Caller owns the deadline.
pub fn server_handshake<S: Read + Write>(
    stream: &mut S,
    psk: &[u8],
    server_nonce: [u8; NONCE_LEN],
) -> Result<SessionKeys, ProtocolError> {
    handshake::server(stream, psk, server_nonce)
}

fn write_transport_prefix(prefix: &mut [u8]) {
    prefix[..TRANSPORT_MAGIC.len()].copy_from_slice(TRANSPORT_MAGIC);
    prefix[TRANSPORT_MAGIC.len()..].copy_from_slice(&TRANSPORT_VERSION.to_be_bytes());
}

fn check_transport_prefix(prefix: &[u8]) -> Result<(), ProtocolError> {
    if &prefix[..TRANSPORT_MAGIC.len()] != TRANSPORT_MAGIC {
        return Err(ProtocolError::BadTransportMagic);
    }
    let received = u16::from_be_bytes(
        prefix[TRANSPORT_MAGIC.len()..]
            .try_into()
            .expect("the transport version field has a fixed length"),
    );
    if received != TRANSPORT_VERSION {
        return Err(ProtocolError::UnsupportedTransportVersion {
            received,
            expected: TRANSPORT_VERSION,
        });
    }
    Ok(())
}

fn validate_psk(psk: &[u8]) -> Result<(), ProtocolError> {
    if psk.len() != PSK_LEN {
        return Err(ProtocolError::InvalidPskLength {
            expected: PSK_LEN,
            actual: psk.len(),
        });
    }
    Ok(())
}

fn cipher(key: &[u8; MAC_LEN]) -> ChaCha20Poly1305 {
    ChaCha20Poly1305::new(&Key::from(*key))
}

/// Nonce for one record: four zero bytes then the big-endian sequence.
///
/// Sequence numbers start at zero, advance by exactly one, and exhausting
/// `u64::MAX` is a hard error on both the read and the write path, so a
/// (key, nonce) pair is never reused.
fn record_nonce(sequence: u64) -> Nonce {
    let mut nonce = [0_u8; 12];
    nonce[4..].copy_from_slice(&sequence.to_be_bytes());
    Nonce::from(nonce)
}

/// Sealed plaintext length for a payload: `payload || marker`, bucketed.
fn padded_len(payload_len: usize) -> usize {
    let minimum = payload_len.saturating_add(1);
    PADDING_BUCKETS
        .into_iter()
        .find(|bucket| *bucket >= minimum)
        .unwrap_or(minimum)
}

/// Remove trailing padding in place, leaving only the payload.
///
/// The plaintext ends with the marker followed by zero bytes, so scanning back
/// to the last non-zero byte finds the boundary without a length field that
/// would itself have to be authenticated separately.
fn strip_padding(plaintext: &mut Vec<u8>) -> Result<(), ProtocolError> {
    let boundary = plaintext
        .iter()
        .rposition(|byte| *byte != 0)
        .ok_or(ProtocolError::MalformedPadding)?;
    if plaintext[boundary] != PADDING_MARKER {
        return Err(ProtocolError::MalformedPadding);
    }
    plaintext.truncate(boundary);
    Ok(())
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }

    let mut difference = 0_u8;
    for (&left, &right) in left.iter().zip(right) {
        difference |= left ^ right;
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, ErrorKind};
    use std::os::unix::net::UnixStream;
    use std::thread;

    /// Largest authenticated input batch, to pin the top padding bucket.
    const MAX_INPUT_BATCH_PROBE: usize = 641;

    const TEST_PSK: [u8; PSK_LEN] = [0x42; PSK_LEN];

    fn encoded_record(
        key: [u8; MAC_LEN],
        sequence: u64,
        kind: MessageKind,
        payload: &[u8],
    ) -> Vec<u8> {
        let mut writer = SessionWriter::new(Vec::new(), key);
        writer.next_sequence = sequence;
        writer.write_message(kind, payload).unwrap();
        writer.into_inner()
    }

    #[test]
    fn application_message_kinds_remain_a_closed_set() {
        for (tag, kind) in [
            (1, MessageKind::Hello),
            (2, MessageKind::HelloAck),
            (3, MessageKind::Frame),
            (4, MessageKind::Pointer),
            (5, MessageKind::Key),
            (6, MessageKind::Button),
            (7, MessageKind::ReleaseAll),
            (8, MessageKind::Close),
            (9, MessageKind::Heartbeat),
            (10, MessageKind::FrameAck),
            (11, MessageKind::InputBatch),
            (12, MessageKind::Viewport),
            (13, MessageKind::Clipboard),
            (14, MessageKind::ClipboardTransfer),
            (15, MessageKind::CursorShape),
            (16, MessageKind::CursorPosition),
            (17, MessageKind::SourceCatalog),
            (18, MessageKind::SourceRequest),
            (19, MessageKind::SourceChanged),
            (20, MessageKind::SessionClose),
            (21, MessageKind::ClipboardRestrict),
            (22, MessageKind::ClipboardRestricted),
            (23, MessageKind::WindowResizeRequest),
            (24, MessageKind::WindowResizeResult),
            (25, MessageKind::WaylandKeyboardRequest),
            (26, MessageKind::WaylandKeyboardStatus),
            (27, MessageKind::WaylandKeyboardKey),
            (28, MessageKind::WaylandKeyboardRevoke),
            (29, MessageKind::WaylandPointerRequest),
            (30, MessageKind::WaylandPointerStatus),
            (31, MessageKind::WaylandPointerAction),
            (32, MessageKind::WaylandPointerRevoke),
            (33, MessageKind::WaylandControlRequest),
            (34, MessageKind::WaylandControlStatus),
            (35, MessageKind::WaylandControlAction),
            (36, MessageKind::WaylandControlRevoke),
            (37, MessageKind::WaylandTextCommit),
            (38, MessageKind::WaylandTextResult),
        ] {
            assert_eq!(MessageKind::try_from(tag).unwrap(), kind);
            assert_eq!(u8::from(kind), tag);
        }
        for tag in (0..=u8::MAX).filter(|tag| !(1..=38).contains(tag)) {
            assert!(
                matches!(MessageKind::try_from(tag), Err(ProtocolError::UnknownMessageKind(rejected)) if rejected == tag),
                "unallocated application message kind {tag} must be rejected"
            );
        }
    }

    #[test]
    fn record_payloads_are_encrypted_not_merely_authenticated() {
        let key = [0x4d; MAC_LEN];
        // A key press is three plaintext bytes on the wire in v1: tag, keycode,
        // pressed. A passive sniffer could reconstruct a typed password from
        // them byte for byte, with no image analysis at all.
        let secret = b"correct horse battery staple";
        let record = encoded_record(key, 0, MessageKind::InputBatch, secret);
        assert!(
            !record
                .windows(secret.len())
                .any(|window| window == secret.as_slice()),
            "the payload must not appear anywhere in the sealed record"
        );

        let mut reader = SessionReader::new(Cursor::new(record), key);
        let (kind, payload) = reader.read_message().unwrap();
        assert_eq!(kind, MessageKind::InputBatch);
        assert_eq!(payload, secret);
    }

    #[test]
    fn identical_payloads_under_different_sequences_share_no_ciphertext() {
        let key = [0x4e; MAC_LEN];
        let first = encoded_record(key, 0, MessageKind::InputBatch, b"same");
        let second = encoded_record(key, 1, MessageKind::InputBatch, b"same");
        assert_ne!(
            first[RECORD_HEADER_LEN..],
            second[RECORD_HEADER_LEN..],
            "each record must use its own nonce, so repeats are not visible"
        );
    }

    #[test]
    fn small_records_are_padded_into_indistinguishable_buckets() {
        let key = [0x4f; MAC_LEN];
        // A keystroke batch, a pointer run and a release-all have distinct
        // natural lengths; after padding their records are the same size.
        let key_batch = encoded_record(key, 0, MessageKind::InputBatch, &[2, 38, 1]);
        let pointer_run = encoded_record(key, 0, MessageKind::InputBatch, &[1, 0, 40, 0, 90]);
        let release_all = encoded_record(key, 0, MessageKind::InputBatch, &[4]);
        assert_eq!(key_batch.len(), pointer_run.len());
        assert_eq!(key_batch.len(), release_all.len());
        assert_eq!(
            key_batch.len(),
            RECORD_HEADER_LEN + PADDING_BUCKETS[0] + TAG_LEN
        );

        // Padding stops where it stops paying: a screen frame's size is
        // dominated by its content and no bucket could hide that.
        let frame = encoded_record(key, 0, MessageKind::Frame, &vec![7_u8; 4096]);
        assert_eq!(frame.len(), RECORD_HEADER_LEN + 4096 + 1 + TAG_LEN);
    }

    #[test]
    fn every_bucket_boundary_round_trips_exactly() {
        let key = [0x50; MAC_LEN];
        let mut sizes = vec![0_usize, 1, MAX_INPUT_BATCH_PROBE];
        for bucket in PADDING_BUCKETS {
            sizes.extend([bucket - 2, bucket - 1, bucket, bucket + 1]);
        }
        for len in sizes {
            let payload = (0..len)
                .map(|index| (index % 251) as u8)
                .collect::<Vec<_>>();
            let record = encoded_record(key, 0, MessageKind::Hello, &payload);
            let mut reader = SessionReader::new(Cursor::new(record), key);
            let (_, decoded) = reader.read_message().unwrap();
            assert_eq!(
                decoded, payload,
                "payload of {len} bytes must survive padding"
            );
        }
    }

    #[test]
    fn a_payload_ending_in_zero_bytes_is_not_mistaken_for_padding() {
        let key = [0x51; MAC_LEN];
        let payload = vec![0_u8; 300];
        let record = encoded_record(key, 0, MessageKind::Hello, &payload);
        let mut reader = SessionReader::new(Cursor::new(record), key);
        let (_, decoded) = reader.read_message().unwrap();
        assert_eq!(
            decoded, payload,
            "the marker byte, not a zero scan, sets the boundary"
        );
    }

    #[test]
    fn malformed_padding_is_rejected() {
        // Reachable only from a peer holding the key, so it is a correctness
        // guard rather than an attack surface; it must still fail closed.
        assert!(matches!(
            strip_padding(&mut Vec::new()),
            Err(ProtocolError::MalformedPadding)
        ));
        assert!(matches!(
            strip_padding(&mut vec![0_u8; 8]),
            Err(ProtocolError::MalformedPadding)
        ));
        assert!(matches!(
            strip_padding(&mut vec![9_u8, 0, 0]),
            Err(ProtocolError::MalformedPadding)
        ));
        let mut exact = vec![b'h', b'i', PADDING_MARKER, 0, 0];
        strip_padding(&mut exact).unwrap();
        assert_eq!(exact, b"hi");
    }

    #[test]
    fn a_lowered_payload_cap_is_enforced_before_any_allocation() {
        let key = [0x52; MAC_LEN];
        let record = encoded_record(key, 0, MessageKind::Frame, &vec![0x33_u8; 4096]);
        let mut reader = SessionReader::new(Cursor::new(record), key);
        reader.set_max_payload_len(1024);
        assert!(matches!(
            reader.read_message(),
            Err(ProtocolError::PayloadTooLarge { max: 1024, .. })
        ));

        // The cap can only ever tighten; it cannot widen past the global limit.
        let mut widened = SessionReader::new(Cursor::new(Vec::<u8>::new()), key);
        widened.set_max_payload_len(usize::MAX);
        assert_eq!(widened.max_payload_len, MAX_PAYLOAD_LEN);
    }

    #[test]
    fn transport_prefix_rejects_a_stranger_and_a_wrong_version() {
        let mut prefix = [0_u8; TRANSPORT_PREFIX_LEN];
        write_transport_prefix(&mut prefix);
        assert!(check_transport_prefix(&prefix).is_ok());

        let mut wrong_magic = prefix;
        wrong_magic[0] = b'X';
        assert!(matches!(
            check_transport_prefix(&wrong_magic),
            Err(ProtocolError::BadTransportMagic)
        ));

        let mut wrong_version = prefix;
        wrong_version[TRANSPORT_MAGIC.len()..].copy_from_slice(&1_u16.to_be_bytes());
        assert!(matches!(
            check_transport_prefix(&wrong_version),
            Err(ProtocolError::UnsupportedTransportVersion {
                received: 1,
                expected: TRANSPORT_VERSION
            })
        ));
    }

    #[test]
    fn handshake_derives_matching_directional_sessions() {
        let (mut client_stream, mut server_stream) = UnixStream::pair().unwrap();
        for stream in [&client_stream, &server_stream] {
            let timeout = Some(std::time::Duration::from_secs(5));
            stream.set_read_timeout(timeout).unwrap();
            stream.set_write_timeout(timeout).unwrap();
        }

        let server = thread::spawn(move || {
            let keys = server_handshake(&mut server_stream, &TEST_PSK, [0x51; NONCE_LEN]).unwrap();
            let reader_stream = server_stream.try_clone().unwrap();
            let (receive_key, send_key) = keys.into_server();
            let mut reader = SessionReader::new(reader_stream, receive_key);
            let mut writer = SessionWriter::new(server_stream, send_key);
            writer
                .write_message(MessageKind::Frame, b"desktop")
                .unwrap();
            writer.flush().unwrap();

            let (kind, payload) = reader.read_message().unwrap();
            assert_eq!(kind, MessageKind::Key);
            assert_eq!(payload, b"key-down");
        });

        let keys = client_handshake(&mut client_stream, &TEST_PSK, [0x61; NONCE_LEN]).unwrap();
        let reader_stream = client_stream.try_clone().unwrap();
        let (receive_key, send_key) = keys.into_client();
        let mut reader = SessionReader::new(reader_stream, receive_key);
        let mut writer = SessionWriter::new(client_stream, send_key);
        let (kind, payload) = reader.read_message().unwrap();
        assert_eq!(kind, MessageKind::Frame);
        assert_eq!(payload, b"desktop");

        writer.write_message(MessageKind::Key, b"key-down").unwrap();
        writer.flush().unwrap();
        server.join().unwrap();
    }

    #[test]
    fn handshake_rejects_wrong_key() {
        let (mut client_stream, mut server_stream) = UnixStream::pair().unwrap();
        for stream in [&client_stream, &server_stream] {
            let timeout = Some(std::time::Duration::from_secs(5));
            stream.set_read_timeout(timeout).unwrap();
            stream.set_write_timeout(timeout).unwrap();
        }
        let wrong_psk = [0x24; PSK_LEN];

        let server = thread::spawn(move || {
            server_handshake(&mut server_stream, &TEST_PSK, [0x52; NONCE_LEN])
        });
        let client_result = client_handshake(&mut client_stream, &wrong_psk, [0x62; NONCE_LEN]);
        assert!(matches!(
            client_result,
            Err(ProtocolError::AuthenticationFailed)
        ));
        // NNpsk2 rejects the responder's authenticated second flight here.
        // The responder is still awaiting client Finished: honor the public
        // caller contract by closing before joining, rather than deadlocking.
        drop(client_stream);
        let server_result = server.join().unwrap();
        assert!(matches!(
            server_result,
            Err(ProtocolError::Io(ref error))
                if matches!(error.kind(), ErrorKind::UnexpectedEof
                    | ErrorKind::ConnectionReset | ErrorKind::BrokenPipe)
        ));
    }

    #[test]
    fn record_tampering_is_rejected() {
        let key = [0x11; MAC_LEN];
        let mut record = encoded_record(key, 0, MessageKind::Key, b"pointer");
        record[RECORD_HEADER_LEN] ^= 0x80;

        let mut reader = SessionReader::new(Cursor::new(record), key);
        let mut payload = b"previous authenticated payload".to_vec();
        assert!(matches!(
            reader.read_message_into(&mut payload),
            Err(ProtocolError::AuthenticationFailed)
        ));
        assert!(payload.is_empty());
        assert_eq!(reader.expected_sequence(), 0);
    }

    #[test]
    fn authenticated_reads_reuse_the_callers_payload_allocation() {
        let key = [0x16; MAC_LEN];
        let body = vec![0x5a; 4096];
        let mut wire = encoded_record(key, 0, MessageKind::Frame, &body);
        wire.extend_from_slice(&encoded_record(key, 1, MessageKind::Frame, &body));
        wire.extend_from_slice(&encoded_record(key, 2, MessageKind::Close, b"done"));
        let mut reader = SessionReader::new(Cursor::new(wire), key);
        let mut payload = Vec::new();

        assert_eq!(
            reader.read_message_into(&mut payload).unwrap(),
            MessageKind::Frame
        );
        assert_eq!(payload, body);
        let allocation = payload.as_ptr();
        let capacity = payload.capacity();

        assert_eq!(
            reader.read_message_into(&mut payload).unwrap(),
            MessageKind::Frame
        );
        assert_eq!(payload, body);
        assert_eq!(payload.as_ptr(), allocation);
        assert_eq!(payload.capacity(), capacity);
        assert_eq!(
            reader.read_message_into(&mut payload).unwrap(),
            MessageKind::Close
        );
        assert_eq!(payload, b"done");
        assert_eq!(payload.as_ptr(), allocation);
        assert_eq!(payload.capacity(), capacity);
        assert_eq!(reader.expected_sequence(), 3);
    }

    #[test]
    fn replayed_and_out_of_order_records_are_rejected() {
        let key = [0x12; MAC_LEN];
        let first = encoded_record(key, 0, MessageKind::Hello, b"one");
        let mut replayed = first.clone();
        replayed.extend_from_slice(&first);
        let mut replay_reader = SessionReader::new(Cursor::new(replayed), key);
        replay_reader.read_message().unwrap();
        assert!(matches!(
            replay_reader.read_message(),
            Err(ProtocolError::SequenceMismatch {
                expected: 1,
                received: 0
            })
        ));

        let second = encoded_record(key, 1, MessageKind::Hello, b"two");
        let mut out_of_order_reader = SessionReader::new(Cursor::new(second), key);
        assert!(matches!(
            out_of_order_reader.read_message(),
            Err(ProtocolError::SequenceMismatch {
                expected: 0,
                received: 1
            })
        ));
    }

    #[test]
    fn oversized_payload_is_rejected_before_payload_read() {
        let mut header = [0_u8; RECORD_HEADER_LEN];
        header[0] = MessageKind::Frame.into();
        header[1..9].copy_from_slice(&0_u64.to_be_bytes());
        header[9..13].copy_from_slice(&((MAX_PAYLOAD_LEN as u32) + 1).to_be_bytes());

        let mut reader = SessionReader::new(Cursor::new(header), [0x13; MAC_LEN]);
        let mut payload = Vec::with_capacity(64);
        payload.extend_from_slice(b"old");
        let capacity = payload.capacity();
        assert!(matches!(
            reader.read_message_into(&mut payload),
            Err(ProtocolError::PayloadTooLarge {
                len,
                max: MAX_PAYLOAD_LEN
            }) if len == MAX_PAYLOAD_LEN + 1
        ));
        assert!(payload.is_empty());
        assert_eq!(payload.capacity(), capacity);
        assert_eq!(reader.expected_sequence(), 0);

        let mut oversized_retained = Vec::with_capacity(RETAINED_PAYLOAD_SOFT_LIMIT + 1);
        oversized_retained.push(1);
        let mut reader = SessionReader::new(Cursor::new(header), [0x13; MAC_LEN]);
        assert!(reader.read_message_into(&mut oversized_retained).is_err());
        assert!(oversized_retained.is_empty());
        assert_eq!(oversized_retained.capacity(), 0);
    }

    #[test]
    fn truncated_record_is_reported_as_unexpected_eof() {
        let key = [0x14; MAC_LEN];
        let mut record = encoded_record(key, 0, MessageKind::Close, b"bye");
        record.pop();

        let mut reader = SessionReader::new(Cursor::new(record), key);
        let mut payload = b"previous".to_vec();
        match reader.read_message_into(&mut payload) {
            Err(ProtocolError::Io(error)) => {
                assert_eq!(error.kind(), ErrorKind::UnexpectedEof);
            }
            other => panic!("expected truncated-record I/O error, got {other:?}"),
        }
        assert!(payload.is_empty());
        assert_eq!(reader.expected_sequence(), 0);
    }

    #[test]
    fn truncated_payload_is_cleared_and_does_not_advance_sequence() {
        let key = [0x17; MAC_LEN];
        let mut record = encoded_record(key, 0, MessageKind::Frame, b"complete");
        record.truncate(RECORD_HEADER_LEN + 3);
        let mut reader = SessionReader::new(Cursor::new(record), key);
        let mut payload = b"old".to_vec();

        assert!(matches!(
            reader.read_message_into(&mut payload),
            Err(ProtocolError::Io(ref error)) if error.kind() == ErrorKind::UnexpectedEof
        ));
        assert!(payload.is_empty());
        assert_eq!(reader.expected_sequence(), 0);
    }

    #[test]
    fn payload_capacity_shrinks_only_after_sustained_underuse() {
        let mut payload = Vec::with_capacity(RETAINED_PAYLOAD_SOFT_LIMIT * 2);
        payload.push(1);
        let original_capacity = payload.capacity();
        let mut retention = PayloadBufferRetention::default();

        for expected in 1..UNDERUSED_PAYLOADS_BEFORE_SHRINK {
            assert!(!retention.should_shrink(1, original_capacity));
            assert_eq!(retention.underused_payloads, expected);
        }
        assert!(retention.should_shrink(1, original_capacity));
        assert_eq!(retention.underused_payloads, 0);

        retention.observe(&mut payload);
        assert_eq!(retention.underused_payloads, 1);
        for _ in 1..UNDERUSED_PAYLOADS_BEFORE_SHRINK {
            retention.observe(&mut payload);
        }
        assert_eq!(retention.underused_payloads, 0);
        assert!(payload.capacity() < original_capacity);

        payload.resize(payload.capacity() / 2, 0);
        retention.observe(&mut payload);
        assert_eq!(retention.underused_payloads, 0);
    }

    #[test]
    fn recently_added_message_kinds_have_stable_wire_values() {
        assert_eq!(u8::from(MessageKind::FrameAck), 10);
        assert_eq!(MessageKind::try_from(10).unwrap(), MessageKind::FrameAck);
        assert_eq!(u8::from(MessageKind::InputBatch), 11);
        assert_eq!(MessageKind::try_from(11).unwrap(), MessageKind::InputBatch);
    }

    #[test]
    fn unknown_message_kind_is_a_protocol_error() {
        let key = [0x15; MAC_LEN];
        // Seal a genuinely well-formed record whose kind byte is not a known
        // kind, so the failure under test is the kind and not authentication.
        let sealed_len = padded_len(0);
        let mut header = [0_u8; RECORD_HEADER_LEN];
        header[0] = 0xff;
        header[9..13].copy_from_slice(&(sealed_len as u32).to_be_bytes());
        let mut sealed = vec![0_u8; sealed_len];
        sealed[0] = PADDING_MARKER;
        let tag = cipher(&key)
            .encrypt_inout_detached(&record_nonce(0), &header, sealed.as_mut_slice().into())
            .unwrap();
        let mut record = header.to_vec();
        record.extend_from_slice(&sealed);
        record.extend_from_slice(&tag);

        let mut reader = SessionReader::new(Cursor::new(record), key);
        assert!(matches!(
            reader.read_message(),
            Err(ProtocolError::UnknownMessageKind(0xff))
        ));
        assert_eq!(reader.expected_sequence(), 0);
    }

    #[test]
    fn unauthenticated_unknown_kind_is_not_parsed_or_exposed() {
        let key = [0x18; MAC_LEN];
        let mut record = vec![0_u8; RECORD_HEADER_LEN + TAG_LEN];
        record[0] = 0xff;
        let mut reader = SessionReader::new(Cursor::new(record), key);
        let mut payload = b"old".to_vec();

        assert!(matches!(
            reader.read_message_into(&mut payload),
            Err(ProtocolError::AuthenticationFailed)
        ));
        assert!(payload.is_empty());
        assert_eq!(reader.expected_sequence(), 0);
    }

    #[test]
    fn invalid_psk_length_is_rejected_before_io() {
        let mut stream = Cursor::new(Vec::<u8>::new());
        let result = client_handshake(&mut stream, b"short", [0x63; NONCE_LEN]);
        assert!(matches!(
            result,
            Err(ProtocolError::InvalidPskLength {
                expected: PSK_LEN,
                actual: 5
            })
        ));
    }

    #[test]
    fn directional_keys_are_distinct_and_not_interchangeable() {
        // Distinct authenticated keys cannot open the opposite record direction.
        // Real Noise envelope key agreement is covered by handshake tests.
        let c2s = [0x20; MAC_LEN];
        let s2c = [0x30; MAC_LEN];
        assert_ne!(c2s, s2c);

        let record = encoded_record(c2s, 0, MessageKind::Hello, b"probe");
        let mut wrong_direction = SessionReader::new(Cursor::new(record), s2c);
        assert!(matches!(
            wrong_direction.read_message(),
            Err(ProtocolError::AuthenticationFailed)
        ));
    }
}

#[cfg(test)]
#[path = "protocol_noise_tests.rs"]
mod noise_tests;

#[cfg(test)]
#[path = "protocol_noise_tcp_tests.rs"]
mod noise_tcp_tests;
