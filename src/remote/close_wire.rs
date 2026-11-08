//! Bounded, authenticated host termination. Codes explain why the peer ended
//! the session; they never authorize reconnect or certify either side's cleanup.

use super::RemoteResult;
use std::io;

const VERSION: u8 = 1;
const HEADER_LEN: usize = 4;
pub const MAX_REASON_BYTES: usize = 240;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum SessionCloseCode {
    Busy = 1,
    AdmissionDisabled = 2,
    CaptureUnavailable = 3,
    ClipboardUnavailable = 4,
    HostStopping = 5,
}

impl TryFrom<u8> for SessionCloseCode {
    type Error = io::Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Busy),
            2 => Ok(Self::AdmissionDisabled),
            3 => Ok(Self::CaptureUnavailable),
            4 => Ok(Self::ClipboardUnavailable),
            5 => Ok(Self::HostStopping),
            _ => Err(invalid("unknown session close code")),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionClose {
    pub code: SessionCloseCode,
    /// Untrusted display text, deliberately excluded from machine diagnostics.
    pub reason: String,
}

impl SessionClose {
    pub fn new(code: SessionCloseCode, reason: &str) -> Self {
        Self {
            code,
            reason: bounded_reason(reason).to_owned(),
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        // Bound at encoding too, even if a caller modified the public reason.
        let reason = bounded_reason(&self.reason);
        let mut payload = Vec::with_capacity(HEADER_LEN + reason.len());
        payload.extend_from_slice(&[VERSION, self.code as u8]);
        payload.extend_from_slice(&(reason.len() as u16).to_be_bytes());
        payload.extend_from_slice(reason.as_bytes());
        payload
    }

    pub fn decode(payload: &[u8]) -> RemoteResult<Self> {
        if payload.len() < HEADER_LEN || payload.len() > HEADER_LEN + MAX_REASON_BYTES {
            return Err(invalid("invalid session close length").into());
        }
        if payload[0] != VERSION {
            return Err(invalid("unsupported session close version").into());
        }
        let code = SessionCloseCode::try_from(payload[1])?;
        let len = usize::from(u16::from_be_bytes([payload[2], payload[3]]));
        if len > MAX_REASON_BYTES || payload.len() != HEADER_LEN + len {
            return Err(invalid("invalid session close reason length").into());
        }
        let reason = std::str::from_utf8(&payload[HEADER_LEN..])
            .map_err(|_| invalid("invalid session close reason UTF-8"))?
            .to_owned();
        Ok(Self { code, reason })
    }
}

fn bounded_reason(reason: &str) -> &str {
    let mut end = reason.len().min(MAX_REASON_BYTES);
    while !reason.is_char_boundary(end) {
        end -= 1;
    }
    &reason[..end]
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_codes_roundtrip_with_fixed_diagnostic_names() {
        for (tag, code, name) in [
            (1, SessionCloseCode::Busy, "busy"),
            (2, SessionCloseCode::AdmissionDisabled, "admission_disabled"),
            (
                3,
                SessionCloseCode::CaptureUnavailable,
                "capture_unavailable",
            ),
            (
                4,
                SessionCloseCode::ClipboardUnavailable,
                "clipboard_unavailable",
            ),
            (5, SessionCloseCode::HostStopping, "host_stopping"),
        ] {
            assert_eq!(SessionCloseCode::try_from(tag).unwrap(), code);
            assert_eq!(serde_json::to_value(code).unwrap(), name);
            for reason in ["", "host stopped", "结束 🖥\n\u{1b}"] {
                let close = SessionClose::new(code, reason);
                let payload = close.encode();
                assert_eq!(payload[..4], [VERSION, tag, 0, reason.len() as u8]);
                assert_eq!(SessionClose::decode(&payload).unwrap(), close);
            }
        }
    }

    #[test]
    fn reason_bound_is_bytes_and_preserves_utf8() {
        for prefix in 0..=MAX_REASON_BYTES + 4 {
            for suffix in ["é", "界", "🖥"] {
                let reason = format!("{}{suffix}tail", "x".repeat(prefix));
                let close = SessionClose::new(SessionCloseCode::Busy, &reason);
                assert!(close.reason.len() <= MAX_REASON_BYTES);
                assert!(reason.starts_with(&close.reason));
                let decoded = SessionClose::decode(&close.encode()).unwrap();
                assert_eq!(decoded, close);
                // Directly constructing a value cannot bypass the encoder cap.
                let direct = SessionClose { reason, ..close };
                assert_eq!(SessionClose::decode(&direct.encode()).unwrap(), decoded);
            }
        }
        let payload = SessionClose::new(SessionCloseCode::Busy, &"x".repeat(240)).encode();
        assert_eq!(payload.len(), 244);
        assert_eq!(&payload[2..4], &[0, 240]);
    }

    #[test]
    fn malformed_versions_codes_lengths_and_utf8_are_rejected() {
        let valid = SessionClose::new(SessionCloseCode::Busy, "x").encode();
        for version in 0..=255 {
            if version != VERSION {
                let mut bad = valid.clone();
                bad[0] = version;
                assert!(SessionClose::decode(&bad).is_err());
            }
        }
        for code in 0..=255 {
            if !(1..=5).contains(&code) {
                let mut bad = valid.clone();
                bad[1] = code;
                assert!(SessionClose::decode(&bad).is_err());
            }
        }
        for len in 0..valid.len() {
            assert!(SessionClose::decode(&valid[..len]).is_err());
        }
        for len in [0, 2, 240, 241, u16::MAX] {
            let mut bad = valid.clone();
            bad[2..4].copy_from_slice(&len.to_be_bytes());
            assert!(SessionClose::decode(&bad).is_err());
        }
        let mut trailing = valid.clone();
        trailing.push(0);
        assert!(SessionClose::decode(&trailing).is_err());
        for reason in [vec![0xff], vec![0xc0, 0x80], vec![0xe4, 0xb8]] {
            let mut bad = vec![VERSION, 1];
            bad.extend_from_slice(&(reason.len() as u16).to_be_bytes());
            bad.extend_from_slice(&reason);
            assert!(SessionClose::decode(&bad).is_err());
        }
        let mut oversized = vec![VERSION, 1, 0, 241];
        oversized.extend_from_slice(&[b'x'; 241]);
        assert!(SessionClose::decode(&oversized).is_err());
        assert!(SessionClose::decode(b"busy: retry; cleanup complete").is_err());
    }
}
