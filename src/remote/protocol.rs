//! Authenticated wire protocol for JWM's LAN remote-control transport.
//!
//! The handshake proves possession of a 32-byte pre-shared key and derives
//! independent keys for each traffic direction:
//!
//! ```text
//! server -> client: server_nonce (32 bytes)
//! client -> server: client_nonce (32 bytes) || client_proof (32 bytes)
//! server -> client: server_proof (32 bytes)
//! ```
//!
//! After the handshake, records have the following layout:
//!
//! ```text
//! kind (u8) || sequence (u64, BE) || payload_len (u32, BE)
//!     || payload || HMAC-SHA256 (32 bytes)
//! ```
//!
//! This protocol authenticates peers and records, but it deliberately does
//! **not encrypt payloads**. It is suitable only for the explicitly trusted-LAN
//! MVP. Callers must close the connection after any [`ProtocolError`], and
//! should set handshake/read/write deadlines on their underlying stream.

use std::fmt;
use std::io::{self, Read, Write};

use sha2::{Digest, Sha256};

/// Required size of the pre-shared key.
pub const PSK_LEN: usize = 32;

/// Size of each handshake nonce.
pub const NONCE_LEN: usize = 32;

/// Size of every HMAC-SHA256 proof or record authenticator.
pub const MAC_LEN: usize = 32;

/// Maximum accepted payload size for one authenticated record (32 MiB).
pub const MAX_PAYLOAD_LEN: usize = 32 * 1024 * 1024;

const RECORD_HEADER_LEN: usize = 1 + 8 + 4;
const CLIENT_HANDSHAKE_LEN: usize = NONCE_LEN + MAC_LEN;
const SHA256_BLOCK_LEN: usize = 64;

const CLIENT_PROOF_DOMAIN: &[u8] = b"jwm-remote/v1/client-proof";
const SERVER_PROOF_DOMAIN: &[u8] = b"jwm-remote/v1/server-proof";
const C2S_KEY_DOMAIN: &[u8] = b"jwm-remote/v1/key/client-to-server";
const S2C_KEY_DOMAIN: &[u8] = b"jwm-remote/v1/key/server-to-client";
const RECORD_DOMAIN: &[u8] = b"jwm-remote/v1/record";

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
    /// Pointer motion payload.
    Pointer = 4,
    /// Keyboard press or release payload.
    Key = 5,
    /// Pointer-button press or release payload.
    Button = 6,
    /// Release every key and button held by this session.
    ReleaseAll = 7,
    /// Graceful session shutdown.
    Close = 8,
    /// Client activity heartbeat; video frames provide the reverse heartbeat.
    Heartbeat = 9,
    /// Cumulative acknowledgement of the latest frame drawn by the client.
    FrameAck = 10,
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

#[derive(Debug, Clone, Copy)]
enum Direction {
    ClientToServer,
    ServerToClient,
}

impl Direction {
    fn key_domain(self) -> &'static [u8] {
        match self {
            Self::ClientToServer => C2S_KEY_DOMAIN,
            Self::ServerToClient => S2C_KEY_DOMAIN,
        }
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
    key: [u8; MAC_LEN],
    expected_sequence: u64,
}

impl<R> SessionReader<R> {
    /// Bind a reader to the receive key selected from [`SessionKeys`].
    pub fn new(inner: R, key: [u8; MAC_LEN]) -> Self {
        Self {
            inner,
            key,
            expected_sequence: 0,
        }
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
    /// Read and authenticate the next record.
    ///
    /// The declared payload length is checked before allocating its buffer.
    /// Sequence numbers must begin at zero and increase by exactly one.
    pub fn read_message(&mut self) -> Result<(MessageKind, Vec<u8>), ProtocolError> {
        if self.expected_sequence == u64::MAX {
            return Err(ProtocolError::SequenceExhausted);
        }

        let mut header = [0_u8; RECORD_HEADER_LEN];
        self.inner.read_exact(&mut header)?;

        let kind = MessageKind::try_from(header[0])?;
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
        if payload_len > MAX_PAYLOAD_LEN {
            return Err(ProtocolError::PayloadTooLarge {
                len: payload_len,
                max: MAX_PAYLOAD_LEN,
            });
        }

        let mut payload = vec![0_u8; payload_len];
        self.inner.read_exact(&mut payload)?;

        let mut received_mac = [0_u8; MAC_LEN];
        self.inner.read_exact(&mut received_mac)?;
        let expected_mac = record_mac(&self.key, &header, &payload);
        if !constant_time_eq(&received_mac, &expected_mac) {
            return Err(ProtocolError::AuthenticationFailed);
        }

        self.expected_sequence += 1;
        Ok((kind, payload))
    }
}

/// Authenticated writer for one direction of a remote session.
pub struct SessionWriter<W> {
    inner: W,
    key: [u8; MAC_LEN],
    next_sequence: u64,
}

impl<W> SessionWriter<W> {
    /// Bind a writer to the send key selected from [`SessionKeys`].
    pub fn new(inner: W, key: [u8; MAC_LEN]) -> Self {
        Self {
            inner,
            key,
            next_sequence: 0,
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

        let payload_len = payload.len() as u32;
        let mut header = [0_u8; RECORD_HEADER_LEN];
        header[0] = kind.into();
        header[1..9].copy_from_slice(&self.next_sequence.to_be_bytes());
        header[9..13].copy_from_slice(&payload_len.to_be_bytes());
        let mac = record_mac(&self.key, &header, payload);

        self.inner.write_all(&header)?;
        self.inner.write_all(payload)?;
        self.inner.write_all(&mac)?;
        self.next_sequence += 1;
        Ok(())
    }

    /// Flush the underlying writer.
    pub fn flush(&mut self) -> Result<(), ProtocolError> {
        self.inner.flush()?;
        Ok(())
    }
}

/// Authenticate as the connecting client and return directional session keys.
///
/// `client_nonce` must be newly generated with a cryptographically secure RNG
/// for each connection. The same full-duplex stream is used for both reads and
/// writes during the handshake.
pub fn client_handshake<S: Read + Write>(
    stream: &mut S,
    psk: &[u8],
    client_nonce: [u8; NONCE_LEN],
) -> Result<SessionKeys, ProtocolError> {
    validate_psk(psk)?;

    let mut server_nonce = [0_u8; NONCE_LEN];
    stream.read_exact(&mut server_nonce)?;

    let client_proof = handshake_proof(psk, CLIENT_PROOF_DOMAIN, &server_nonce, &client_nonce);
    let mut response = [0_u8; CLIENT_HANDSHAKE_LEN];
    response[..NONCE_LEN].copy_from_slice(&client_nonce);
    response[NONCE_LEN..].copy_from_slice(&client_proof);
    stream.write_all(&response)?;
    stream.flush()?;

    let mut received_server_proof = [0_u8; MAC_LEN];
    stream.read_exact(&mut received_server_proof)?;
    let expected_server_proof =
        handshake_proof(psk, SERVER_PROOF_DOMAIN, &server_nonce, &client_nonce);
    if !constant_time_eq(&received_server_proof, &expected_server_proof) {
        return Err(ProtocolError::AuthenticationFailed);
    }

    let c2s_key = derive_session_key(psk, Direction::ClientToServer, &server_nonce, &client_nonce);
    let s2c_key = derive_session_key(psk, Direction::ServerToClient, &server_nonce, &client_nonce);
    Ok(SessionKeys {
        c2s: c2s_key,
        s2c: s2c_key,
    })
}

/// Authenticate as the accepting server and return directional session keys.
///
/// `server_nonce` must be newly generated with a cryptographically secure RNG
/// for each connection. The same full-duplex stream is used for both reads and
/// writes during the handshake.
pub fn server_handshake<S: Read + Write>(
    stream: &mut S,
    psk: &[u8],
    server_nonce: [u8; NONCE_LEN],
) -> Result<SessionKeys, ProtocolError> {
    validate_psk(psk)?;

    stream.write_all(&server_nonce)?;
    stream.flush()?;

    let mut response = [0_u8; CLIENT_HANDSHAKE_LEN];
    stream.read_exact(&mut response)?;
    let mut client_nonce = [0_u8; NONCE_LEN];
    client_nonce.copy_from_slice(&response[..NONCE_LEN]);
    let received_client_proof = &response[NONCE_LEN..];
    let expected_client_proof =
        handshake_proof(psk, CLIENT_PROOF_DOMAIN, &server_nonce, &client_nonce);
    if !constant_time_eq(received_client_proof, &expected_client_proof) {
        return Err(ProtocolError::AuthenticationFailed);
    }

    let server_proof = handshake_proof(psk, SERVER_PROOF_DOMAIN, &server_nonce, &client_nonce);
    stream.write_all(&server_proof)?;
    stream.flush()?;

    let c2s_key = derive_session_key(psk, Direction::ClientToServer, &server_nonce, &client_nonce);
    let s2c_key = derive_session_key(psk, Direction::ServerToClient, &server_nonce, &client_nonce);
    Ok(SessionKeys {
        c2s: c2s_key,
        s2c: s2c_key,
    })
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

fn handshake_proof(
    psk: &[u8],
    role_domain: &[u8],
    server_nonce: &[u8; NONCE_LEN],
    client_nonce: &[u8; NONCE_LEN],
) -> [u8; MAC_LEN] {
    hmac_sha256(psk, &[role_domain, server_nonce, client_nonce])
}

fn derive_session_key(
    psk: &[u8],
    direction: Direction,
    server_nonce: &[u8; NONCE_LEN],
    client_nonce: &[u8; NONCE_LEN],
) -> [u8; MAC_LEN] {
    hmac_sha256(psk, &[direction.key_domain(), server_nonce, client_nonce])
}

fn record_mac(
    key: &[u8; MAC_LEN],
    header: &[u8; RECORD_HEADER_LEN],
    payload: &[u8],
) -> [u8; MAC_LEN] {
    hmac_sha256(key, &[RECORD_DOMAIN, header, payload])
}

/// RFC 2104 HMAC instantiated with SHA-256.
fn hmac_sha256(key: &[u8], parts: &[&[u8]]) -> [u8; MAC_LEN] {
    let mut key_block = [0_u8; SHA256_BLOCK_LEN];
    if key.len() > SHA256_BLOCK_LEN {
        let key_hash = Sha256::digest(key);
        key_block[..MAC_LEN].copy_from_slice(&key_hash);
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }

    let mut inner_pad = [0x36_u8; SHA256_BLOCK_LEN];
    let mut outer_pad = [0x5c_u8; SHA256_BLOCK_LEN];
    for ((inner, outer), key_byte) in inner_pad
        .iter_mut()
        .zip(outer_pad.iter_mut())
        .zip(key_block)
    {
        *inner ^= key_byte;
        *outer ^= key_byte;
    }

    let mut inner = Sha256::new();
    inner.update(inner_pad);
    for part in parts {
        inner.update(part);
    }
    let inner_hash = inner.finalize();

    let mut outer = Sha256::new();
    outer.update(outer_pad);
    outer.update(inner_hash);
    outer.finalize().into()
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

    const TEST_PSK: [u8; PSK_LEN] = [0x42; PSK_LEN];

    fn decode_hex(hex: &str) -> Vec<u8> {
        assert_eq!(hex.len() % 2, 0);
        hex.as_bytes()
            .chunks_exact(2)
            .map(|pair| {
                let high = (pair[0] as char).to_digit(16).unwrap();
                let low = (pair[1] as char).to_digit(16).unwrap();
                ((high << 4) | low) as u8
            })
            .collect()
    }

    fn encoded_record(
        key: [u8; MAC_LEN],
        sequence: u64,
        kind: MessageKind,
        payload: &[u8],
    ) -> Vec<u8> {
        let mut writer = SessionWriter {
            inner: Vec::new(),
            key,
            next_sequence: sequence,
        };
        writer.write_message(kind, payload).unwrap();
        writer.into_inner()
    }

    #[test]
    fn hmac_sha256_matches_rfc_4231_vectors() {
        let case_one = hmac_sha256(&[0x0b; 20], &[b"Hi There"]);
        assert_eq!(
            case_one.as_slice(),
            decode_hex("b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7")
        );

        let case_two = hmac_sha256(b"Jefe", &[b"what do ya want for nothing?"]);
        assert_eq!(
            case_two.as_slice(),
            decode_hex("5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843")
        );

        let long_key = [0xaa; 131];
        let case_six = hmac_sha256(
            &long_key,
            &[b"Test Using Larger Than Block-Size Key - Hash Key First"],
        );
        assert_eq!(
            case_six.as_slice(),
            decode_hex("60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54")
        );
    }

    #[test]
    fn handshake_derives_matching_directional_sessions() {
        let (mut client_stream, mut server_stream) = UnixStream::pair().unwrap();

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
        let wrong_psk = [0x24; PSK_LEN];

        let server = thread::spawn(move || {
            server_handshake(&mut server_stream, &TEST_PSK, [0x52; NONCE_LEN])
        });
        let client_result = client_handshake(&mut client_stream, &wrong_psk, [0x62; NONCE_LEN]);
        let server_result = server.join().unwrap();

        assert!(matches!(
            server_result,
            Err(ProtocolError::AuthenticationFailed)
        ));
        assert!(match client_result {
            Err(ProtocolError::AuthenticationFailed) => true,
            Err(ProtocolError::Io(error)) => error.kind() == ErrorKind::UnexpectedEof,
            _ => false,
        });
    }

    #[test]
    fn record_tampering_is_rejected() {
        let key = [0x11; MAC_LEN];
        let mut record = encoded_record(key, 0, MessageKind::Key, b"pointer");
        record[RECORD_HEADER_LEN] ^= 0x80;

        let mut reader = SessionReader::new(Cursor::new(record), key);
        assert!(matches!(
            reader.read_message(),
            Err(ProtocolError::AuthenticationFailed)
        ));
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
        assert!(matches!(
            reader.read_message(),
            Err(ProtocolError::PayloadTooLarge {
                len,
                max: MAX_PAYLOAD_LEN
            }) if len == MAX_PAYLOAD_LEN + 1
        ));
    }

    #[test]
    fn truncated_record_is_reported_as_unexpected_eof() {
        let key = [0x14; MAC_LEN];
        let mut record = encoded_record(key, 0, MessageKind::Close, b"bye");
        record.pop();

        let mut reader = SessionReader::new(Cursor::new(record), key);
        match reader.read_message() {
            Err(ProtocolError::Io(error)) => {
                assert_eq!(error.kind(), ErrorKind::UnexpectedEof);
            }
            other => panic!("expected truncated-record I/O error, got {other:?}"),
        }
    }

    #[test]
    fn frame_ack_message_kind_has_stable_wire_value() {
        assert_eq!(u8::from(MessageKind::FrameAck), 10);
        assert_eq!(MessageKind::try_from(10).unwrap(), MessageKind::FrameAck);
    }

    #[test]
    fn unknown_message_kind_is_a_protocol_error() {
        let mut header = [0_u8; RECORD_HEADER_LEN];
        header[0] = 0xff;

        let mut reader = SessionReader::new(Cursor::new(header), [0x15; MAC_LEN]);
        assert!(matches!(
            reader.read_message(),
            Err(ProtocolError::UnknownMessageKind(0xff))
        ));
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
        let server_nonce = [0x20; NONCE_LEN];
        let client_nonce = [0x30; NONCE_LEN];
        let c2s = derive_session_key(
            &TEST_PSK,
            Direction::ClientToServer,
            &server_nonce,
            &client_nonce,
        );
        let s2c = derive_session_key(
            &TEST_PSK,
            Direction::ServerToClient,
            &server_nonce,
            &client_nonce,
        );
        assert_ne!(c2s, s2c);

        let record = encoded_record(c2s, 0, MessageKind::Hello, b"probe");
        let mut wrong_direction = SessionReader::new(Cursor::new(record), s2c);
        assert!(matches!(
            wrong_direction.read_message(),
            Err(ProtocolError::AuthenticationFailed)
        ));
    }
}
