//! Real owned TCP transport-v3 regression tests. Run in CI, not a display test.
//! Application records are sent only after the production handshake returns.
use super::*;
use crate::remote::deadline::TcpStreamDeadline;
use std::net::{Shutdown, TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

// Public transport-v3 preface: five magic bytes, u16 version, 32-byte nonce.
const PUBLIC_HEADER_LEN: usize = 5 + 2 + NONCE_LEN;
const PSK: [u8; PSK_LEN] = [0x97; PSK_LEN];
const RECORD_SIZES: [usize; 8] = [0, 63, 64, 255, 256, 703, 704, 65_537];

fn tcp_pair() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (server, _) = listener.accept().unwrap();
    for stream in [&client, &server] {
        stream.set_nodelay(true).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
    }
    (client, server)
}

fn full_session() -> SessionKeys {
    let (mut client, mut server) = tcp_pair();
    let peer = thread::spawn(move || {
        let mut deadline = TcpStreamDeadline::arm(&server, Duration::from_secs(5)).unwrap();
        let keys = server_handshake(&mut server, &PSK, [0x31; NONCE_LEN]).unwrap();
        deadline.cancel();
        let mut reader = SessionReader::new(server.try_clone().unwrap(), keys.c2s);
        let mut writer = SessionWriter::new(server, keys.s2c);
        assert_eq!(
            reader.read_message().unwrap(),
            (MessageKind::Hello, b"application-v23".to_vec())
        );
        writer
            .write_message(MessageKind::HelloAck, b"application-v23")
            .unwrap();
        writer.flush().unwrap();
        for (index, size) in RECORD_SIZES.into_iter().enumerate() {
            let expected = vec![index as u8; size];
            assert_eq!(
                reader.read_message().unwrap(),
                (MessageKind::Clipboard, expected.clone())
            );
            writer.write_message(MessageKind::Frame, &expected).unwrap();
            writer.flush().unwrap();
        }
        assert_eq!(reader.expected_sequence(), 9);
        assert_eq!(writer.next_sequence(), 9);
        keys
    });
    let mut deadline = TcpStreamDeadline::arm(&client, Duration::from_secs(5)).unwrap();
    let keys = client_handshake(&mut client, &PSK, [0x52; NONCE_LEN]).unwrap();
    deadline.cancel();
    let mut reader = SessionReader::new(client.try_clone().unwrap(), keys.s2c);
    let mut writer = SessionWriter::new(client, keys.c2s);
    writer
        .write_message(MessageKind::Hello, b"application-v23")
        .unwrap();
    writer.flush().unwrap();
    assert_eq!(
        reader.read_message().unwrap(),
        (MessageKind::HelloAck, b"application-v23".to_vec())
    );
    for (index, size) in RECORD_SIZES.into_iter().enumerate() {
        let expected = vec![index as u8; size];
        writer
            .write_message(MessageKind::Clipboard, &expected)
            .unwrap();
        writer.flush().unwrap();
        assert_eq!(
            reader.read_message().unwrap(),
            (MessageKind::Frame, expected)
        );
    }
    assert_eq!(reader.expected_sequence(), 9);
    assert_eq!(writer.next_sequence(), 9);
    let remote = peer.join().unwrap();
    assert!(
        keys.c2s == remote.c2s && keys.s2c == remote.s2c,
        "directional keys must agree"
    );
    assert!(keys.c2s != keys.s2c, "directions require distinct keys");
    keys
}

#[test]
fn noise_tcp_complete_sessions_preserve_record_boundaries_and_never_reuse_keys() {
    assert_eq!(TRANSPORT_VERSION, 3);
    let first = full_session();
    // Deliberately repeat public nonces: fresh Noise ephemeral contributions
    // and transport-key material must still prevent session-key reuse.
    let second = full_session();
    assert!(first.c2s != second.c2s && first.s2c != second.s2c);
    let mut old = SessionWriter::new(Vec::new(), first.c2s);
    old.write_message(MessageKind::Hello, b"old authenticated session")
        .unwrap();
    let mut new = SessionReader::new(std::io::Cursor::new(old.into_inner()), second.c2s);
    assert!(
        new.read_message().is_err(),
        "old ciphertext must not authenticate in new session"
    );
}

// Authentication assertions remain separate. Once that handshake has ended,
// an already-closed peer makes shutdown idempotently complete; no other I/O
// failure is an acceptable teardown result.
fn completed_teardown(result: io::Result<()>) -> io::Result<()> {
    match result {
        Err(error) if error.kind() == io::ErrorKind::NotConnected => Ok(()),
        other => other,
    }
}

#[test]
fn noise_teardown_accepts_only_success_or_already_disconnected() {
    assert!(completed_teardown(Ok(())).is_ok());
    assert!(completed_teardown(Err(io::ErrorKind::NotConnected.into())).is_ok());
    for kind in [
        io::ErrorKind::PermissionDenied,
        io::ErrorKind::BrokenPipe,
        io::ErrorKind::ConnectionReset,
        io::ErrorKind::TimedOut,
        io::ErrorKind::Interrupted,
        io::ErrorKind::Other,
    ] {
        let result = completed_teardown(Err(io::Error::new(kind, "original teardown failure")));
        let error = result.expect_err("unrelated teardown errors must remain failures");
        assert_eq!(error.kind(), kind);
        assert_eq!(error.to_string(), "original teardown failure");
    }
}

#[test]
fn noise_tcp_wrong_psk_never_reaches_application_hello() {
    let (mut client, mut server) = tcp_pair();
    let peer = thread::spawn(move || {
        let result = server_handshake(&mut server, &PSK, [0x31; NONCE_LEN]);
        // Production callers must close on authentication failure, not retry
        // or start parsing Hello under unauthenticated/provisional keys.
        let authenticated = result.is_ok();
        completed_teardown(server.shutdown(Shutdown::Both)).unwrap();
        authenticated
    });
    let result = client_handshake(&mut client, &[0x96; PSK_LEN], [0x52; NONCE_LEN]);
    assert!(
        result.is_err(),
        "wrong PSK must not expose keys for an application Hello"
    );
    // NNpsk2 may already have queued its encrypted server flights. They are
    // not application records: close immediately rather than interpreting or
    // draining those unauthenticated handshake bytes as Hello/HelloAck.
    completed_teardown(client.shutdown(Shutdown::Both)).unwrap();
    assert!(
        !peer.join().unwrap(),
        "server must not enter application record phase"
    );
}

/// Feed genuine production-generated Noise bytes but interrupt the first
/// encrypted phase after the complete 39-byte public client prefix. No mock
/// successful Noise state or made-up handshake transcript is involved.
struct InterruptedClient {
    stream: TcpStream,
    sent: usize,
    truncate_at: Option<usize>,
    drip: bool,
}
impl Read for InterruptedClient {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.stream.read(bytes)
    }
}
impl Write for InterruptedClient {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.truncate_at.is_some_and(|limit| self.sent >= limit) {
            self.stream.shutdown(Shutdown::Write)?;
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "intentional flight truncation",
            ));
        }
        let mut count = bytes.len();
        if self.sent < PUBLIC_HEADER_LEN {
            count = count.min(PUBLIC_HEADER_LEN - self.sent);
        } else if self.drip {
            thread::sleep(Duration::from_millis(30));
            count = count.min(1);
        }
        if let Some(limit) = self.truncate_at {
            count = count.min(limit - self.sent);
        }
        let n = self.stream.write(&bytes[..count])?;
        self.sent += n;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

#[test]
fn noise_tcp_valid_prefix_slow_noise_flight_cannot_extend_absolute_deadline() {
    let (client, mut server) = tcp_pair();
    let peer = thread::spawn(move || {
        let _deadline = TcpStreamDeadline::arm(&server, Duration::from_millis(250)).unwrap();
        let started = Instant::now();
        let failed = server_handshake(&mut server, &PSK, [0x31; NONCE_LEN]).is_err();
        (failed, started.elapsed())
    });
    let mut slow = InterruptedClient {
        stream: client,
        sent: 0,
        truncate_at: None,
        drip: true,
    };
    assert!(client_handshake(&mut slow, &PSK, [0x52; NONCE_LEN]).is_err());
    assert!(
        slow.sent >= PUBLIC_HEADER_LEN,
        "complete valid public prefix was sent"
    );
    let (failed, elapsed) = peer.join().unwrap();
    assert!(failed);
    assert!(
        elapsed < Duration::from_secs(3),
        "absolute deadline was renewed by valid Noise bytes: {elapsed:?}"
    );
}

#[test]
fn noise_tcp_truncated_real_first_flight_fails_closed_without_session_keys() {
    // Include length-prefix truncation and an incomplete genuine Noise body.
    for cutoff in [PUBLIC_HEADER_LEN + 1, PUBLIC_HEADER_LEN + 12] {
        let (client, mut server) = tcp_pair();
        let peer =
            thread::spawn(move || server_handshake(&mut server, &PSK, [0x31; NONCE_LEN]).is_err());
        let mut cut = InterruptedClient {
            stream: client,
            sent: 0,
            truncate_at: Some(cutoff),
            drip: false,
        };
        assert!(client_handshake(&mut cut, &PSK, [0x52; NONCE_LEN]).is_err());
        assert_eq!(cut.sent, cutoff);
        assert!(
            peer.join().unwrap(),
            "truncated flight must not yield server session keys"
        );
    }
}
