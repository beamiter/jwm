//! Real public handshakes over bounded in-memory byte pipes. No sockets.
use super::*;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

const PSK: [u8; PSK_LEN] = [0x93; PSK_LEN];
#[derive(Clone, Default)]
enum Fault {
    #[default]
    None,
    Flip(usize),
    Cut(usize),
    Replace(usize, Vec<u8>),
}
struct Pipe {
    rx: mpsc::Receiver<Vec<u8>>,
    tx: Option<mpsc::Sender<Vec<u8>>>,
    pending: VecDeque<u8>,
    sent: usize,
    fault: Fault,
    transcript: Arc<Mutex<Vec<u8>>>,
}
impl Read for Pipe {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        while self.pending.is_empty() {
            match self.rx.recv_timeout(Duration::from_secs(5)) {
                Ok(bytes) => self.pending.extend(bytes),
                Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(0),
                Err(mpsc::RecvTimeoutError::Timeout) => panic!("handshake memory pipe deadlock"),
            }
        }
        // Deliberately fragment reads independently from the sender's writes.
        let n = out.len().min(self.pending.len()).min(7);
        for byte in &mut out[..n] {
            *byte = self.pending.pop_front().unwrap();
        }
        Ok(n)
    }
}
impl Write for Pipe {
    fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        if input.is_empty() {
            return Ok(0);
        }
        let mut bytes = input.to_vec();
        for (i, byte) in bytes.iter_mut().enumerate() {
            let at = self.sent + i;
            match &self.fault {
                Fault::Flip(position) if *position == at => *byte ^= 1,
                Fault::Replace(start, data) if at >= *start && at - start < data.len() => {
                    *byte = data[at - start]
                }
                _ => {}
            }
        }
        let n = match self.fault {
            Fault::Cut(at) => at.saturating_sub(self.sent).min(bytes.len()),
            _ => bytes.len(),
        };
        bytes.truncate(n);
        self.transcript.lock().unwrap().extend_from_slice(&bytes);
        if !bytes.is_empty() {
            self.tx
                .as_ref()
                .ok_or_else(|| io::Error::from(io::ErrorKind::BrokenPipe))?
                .send(bytes)
                .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))?;
        }
        self.sent += input.len();
        if matches!(self.fault, Fault::Cut(at) if self.sent >= at) {
            self.tx.take();
        }
        Ok(input.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
type Outcome = Result<SessionKeys, ProtocolError>;
fn exchange(
    client_fault: Fault,
    server_fault: Fault,
    client_psk: [u8; PSK_LEN],
) -> (Outcome, Outcome, Vec<u8>, Vec<u8>) {
    let (ct, sr) = mpsc::channel();
    let (st, cr) = mpsc::channel();
    let cl = Arc::new(Mutex::new(Vec::new()));
    let sl = Arc::new(Mutex::new(Vec::new()));
    let mut client = Pipe {
        rx: cr,
        tx: Some(ct),
        pending: VecDeque::new(),
        sent: 0,
        fault: client_fault,
        transcript: cl.clone(),
    };
    let mut server = Pipe {
        rx: sr,
        tx: Some(st),
        pending: VecDeque::new(),
        sent: 0,
        fault: server_fault,
        transcript: sl.clone(),
    };
    let worker = std::thread::spawn(move || server_handshake(&mut server, &PSK, [0x51; NONCE_LEN]));
    let c = client_handshake(&mut client, &client_psk, [0x61; NONCE_LEN]);
    drop(client);
    let s = worker.join().expect("server handshake must not panic");
    let client_wire = cl.lock().unwrap().clone();
    let server_wire = sl.lock().unwrap().clone();
    (c, s, client_wire, server_wire)
}
fn keys(result: Outcome) -> SessionKeys {
    result.unwrap_or_else(|e| panic!("honest handshake: {e}"))
}
fn rejected(c: &Outcome, s: &Outcome) {
    assert!(
        c.is_err() && s.is_err(),
        "both peers must reject before returning traffic keys"
    );
    for e in [c.as_ref().err().unwrap(), s.as_ref().err().unwrap()] {
        let message = e.to_string();
        assert!(message.len() < 256, "bounded public error");
        assert!(
            !message.contains("9393939393939393"),
            "PSK must not appear in errors"
        );
    }
}
#[test]
fn noise_memory_handshake_fresh_directional_keys_and_records() {
    let (c, s, _, _) = exchange(Fault::None, Fault::None, PSK);
    let c = keys(c);
    let s = keys(s);
    assert_eq!(c.c2s, s.c2s);
    assert_eq!(c.s2c, s.s2c);
    assert_ne!(c.c2s, c.s2c);
    let (c2, s2, _, _) = exchange(Fault::None, Fault::None, PSK);
    let c2 = keys(c2);
    let s2 = keys(s2);
    assert_ne!(c.c2s, c2.c2s);
    assert_ne!(c.s2c, c2.s2c);
    assert_eq!(c2.c2s, s2.c2s);
    assert_eq!(c2.s2c, s2.s2c);
    let mut writer = SessionWriter::new(Vec::new(), c.c2s);
    writer
        .write_message(MessageKind::Frame, b"authenticated pixels")
        .unwrap();
    let wire = writer.into_inner();
    let mut reader = SessionReader::new(io::Cursor::new(wire.clone()), s.c2s);
    assert_eq!(
        reader.read_message().unwrap(),
        (MessageKind::Frame, b"authenticated pixels".to_vec())
    );
    let mut wrong = SessionReader::new(io::Cursor::new(wire), s.s2c);
    assert!(matches!(
        wrong.read_message(),
        Err(ProtocolError::AuthenticationFailed)
    ));
}
#[test]
fn noise_memory_wrong_psk_and_public_prefix_tamper_fail_closed() {
    let (c, s, _, _) = exchange(Fault::None, Fault::None, [0x92; PSK_LEN]);
    rejected(&c, &s);
    for server in [false, true] {
        for offset in [0, 6, 7, 38] {
            let fault = Fault::Flip(offset);
            let (c, s, _, _) = if server {
                exchange(Fault::None, fault, PSK)
            } else {
                exchange(fault, Fault::None, PSK)
            };
            rejected(&c, &s);
        }
    }
}

// Wire contract is intentionally checked independently of production constants.
// Public prefix 39; each encrypted flight has a two-byte big-endian length.
const C_FINISHED: usize = 39 + 2 + 48;
const S_KEYBOX: usize = 39 + 2 + 48;
const S_FINISHED: usize = S_KEYBOX + 2 + 144;
#[test]
fn noise_memory_flights_have_exact_bounded_framing() {
    let (c, s, cw, sw) = exchange(Fault::None, Fault::None, PSK);
    keys(c);
    keys(s);
    assert_eq!(&cw[..7], b"JWMRT\0\x03");
    assert_eq!(&sw[..7], b"JWMRT\0\x03");
    assert_eq!(cw.len(), C_FINISHED + 2 + 112);
    assert_eq!(sw.len(), S_FINISHED + 2 + 112);
    for (wire, at, len) in [
        (&cw, 39, 48_u16),
        (&cw, C_FINISHED, 112),
        (&sw, 39, 48),
        (&sw, S_KEYBOX, 144),
        (&sw, S_FINISHED, 112),
    ] {
        assert_eq!(&wire[at..at + 2], &len.to_be_bytes());
    }
}
#[test]
fn noise_memory_truncation_and_missing_finished_never_authorize_receiver() {
    for at in [
        0,
        6,
        38,
        39,
        40,
        41,
        C_FINISHED - 1,
        C_FINISHED,
        C_FINISHED + 1,
        C_FINISHED + 2 + 111,
    ] {
        let (c, s, _, _) = exchange(Fault::Cut(at), Fault::None, PSK);
        rejected(&c, &s);
    }
    for at in [
        0,
        6,
        38,
        39,
        40,
        S_KEYBOX - 1,
        S_KEYBOX,
        S_KEYBOX + 1,
        S_FINISHED - 1,
    ] {
        let (c, s, _, _) = exchange(Fault::None, Fault::Cut(at), PSK);
        rejected(&c, &s);
    }
    for at in [S_FINISHED, S_FINISHED + 1, S_FINISHED + 2 + 111] {
        let (c, _, _, _) = exchange(Fault::None, Fault::Cut(at), PSK);
        assert!(
            c.is_err(),
            "client must receive and verify the entire ServerFinished"
        );
        // The server cannot know whether its last successful write arrived.
    }
}
#[test]
fn noise_memory_length_bounds_and_encrypted_tampering_fail_closed() {
    for server in [false, true] {
        let starts = if server {
            vec![39, S_KEYBOX, S_FINISHED]
        } else {
            vec![39, C_FINISHED]
        };
        for at in starts {
            for length in [0_u16, 1, 47, 113, u16::MAX] {
                let fault = Fault::Replace(at, length.to_be_bytes().to_vec());
                let (c, s, _, _) = if server {
                    exchange(Fault::None, fault, PSK)
                } else {
                    exchange(fault, Fault::None, PSK)
                };
                if server && at == S_FINISHED {
                    assert!(c.is_err());
                } else {
                    rejected(&c, &s);
                }
            }
            let fault = Fault::Flip(at + 2);
            let (c, s, _, _) = if server {
                exchange(Fault::None, fault, PSK)
            } else {
                exchange(fault, Fault::None, PSK)
            };
            if server && at == S_FINISHED {
                assert!(c.is_err());
            } else {
                rejected(&c, &s);
            }
        }
    }
}
#[test]
fn noise_memory_replayed_cross_session_flights_and_reflection_are_rejected() {
    let (c, s, cw, sw) = exchange(Fault::None, Fault::None, PSK);
    keys(c);
    keys(s);
    // Identical caller-supplied public nonces do not permit replay: fresh
    // Noise ephemeral keys still bind each complete authenticated exchange.
    for (server, start, bytes) in [
        (false, 39, cw[39..C_FINISHED].to_vec()),
        (false, C_FINISHED, cw[C_FINISHED..].to_vec()),
        (true, 39, sw[39..S_KEYBOX].to_vec()),
        (true, S_KEYBOX, sw[S_KEYBOX..S_FINISHED].to_vec()),
        (true, S_FINISHED, sw[S_FINISHED..].to_vec()),
        // Same-size handshake flight reflected into the opposite role.
        (false, 39, sw[39..S_KEYBOX].to_vec()),
        (true, 39, cw[39..C_FINISHED].to_vec()),
        // Same-size Finished ciphertext has the opposite key, role and nonce.
        (false, C_FINISHED, sw[S_FINISHED..].to_vec()),
        (true, S_FINISHED, cw[C_FINISHED..].to_vec()),
        // Out-of-order keybox in the handshake-flight slot.
        (true, 39, sw[S_KEYBOX..S_FINISHED].to_vec()),
    ] {
        let fault = Fault::Replace(start, bytes);
        let (c, s, _, _) = if server {
            exchange(Fault::None, fault, PSK)
        } else {
            exchange(fault, Fault::None, PSK)
        };
        if server && start == S_FINISHED {
            assert!(c.is_err());
        } else {
            rejected(&c, &s);
        }
    }
}

#[test]
fn noise_memory_old_and_unknown_versions_are_rejected_without_fallback() {
    for version in [0_u16, 1, 2, 4, u16::MAX] {
        for server in [false, true] {
            let fault = Fault::Replace(5, version.to_be_bytes().to_vec());
            let (c, s, _, _) = if server {
                exchange(Fault::None, fault, PSK)
            } else {
                exchange(fault, Fault::None, PSK)
            };
            rejected(&c, &s);
            let receiver = if server { &c } else { &s };
            assert!(
                matches!(receiver, Err(ProtocolError::UnsupportedTransportVersion {received,expected:3}) if *received==version)
            );
        }
    }
}
