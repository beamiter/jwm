//! Explicit one-shot text, never keyboard synthesis or clipboard fallback.
use super::RemoteResult;
use super::wayland_keyboard_wire::DrawnScope;
use std::{fmt, io};
pub const MAX_TEXT_BYTES: usize = 4000;
fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "invalid bounded one-shot text record",
    )
}
#[derive(Clone, PartialEq, Eq)]
pub struct ExplicitText(Vec<u8>);
impl fmt::Debug for ExplicitText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExplicitText")
            .field("bytes", &self.0.len())
            .finish_non_exhaustive()
    }
}
impl ExplicitText {
    pub fn new(bytes: Vec<u8>) -> RemoteResult<Self> {
        if bytes.is_empty()
            || bytes.len() > MAX_TEXT_BYTES
            || bytes.contains(&0)
            || std::str::from_utf8(&bytes).is_err()
        {
            return Err(invalid().into());
        }
        Ok(Self(bytes))
    }
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}
fn scope(bytes: &[u8]) -> RemoteResult<DrawnScope> {
    if bytes.len() < 24 {
        return Err(invalid().into());
    }
    let read = |i| u64::from_be_bytes(bytes[i..i + 8].try_into().unwrap());
    let scope = DrawnScope {
        source_epoch: read(0),
        geometry_epoch: read(8),
        frame_sequence: read(16),
    };
    if scope.source_epoch == 0 || scope.geometry_epoch == 0 {
        return Err(invalid().into());
    }
    Ok(scope)
}
fn header(value: DrawnScope) -> RemoteResult<Vec<u8>> {
    let mut bytes = Vec::with_capacity(26);
    for v in [
        value.source_epoch,
        value.geometry_epoch,
        value.frame_sequence,
    ] {
        bytes.extend_from_slice(&v.to_be_bytes());
    }
    scope(&bytes)?;
    Ok(bytes)
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextCommit {
    pub scope: DrawnScope,
    pub text: ExplicitText,
}
impl TextCommit {
    pub fn encode(&self) -> RemoteResult<Vec<u8>> {
        let mut b = header(self.scope)?;
        b.extend_from_slice(&(self.text.0.len() as u16).to_be_bytes());
        b.extend_from_slice(&self.text.0);
        Ok(b)
    }
    pub fn decode(b: &[u8]) -> RemoteResult<Self> {
        if b.len() < 26
            || b.len() > 26 + MAX_TEXT_BYTES
            || usize::from(u16::from_be_bytes(b[24..26].try_into().unwrap())) != b.len() - 26
        {
            return Err(invalid().into());
        }
        Ok(Self {
            scope: scope(b)?,
            text: ExplicitText::new(b[26..].to_vec())?,
        })
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextResult {
    pub scope: DrawnScope,
    pub accepted: bool,
}
impl TextResult {
    pub fn encode(self) -> RemoteResult<Vec<u8>> {
        let mut b = header(self.scope)?;
        b.push(u8::from(self.accepted));
        Ok(b)
    }
    pub fn decode(b: &[u8]) -> RemoteResult<Self> {
        if b.len() != 25 || b[24] > 1 {
            return Err(invalid().into());
        }
        Ok(Self {
            scope: scope(b)?,
            accepted: b[24] == 1,
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_unicode_and_redacted_debug() {
        let value = "中文e\u{301}🙂\n";
        let text = ExplicitText::new(value.as_bytes().to_vec()).unwrap();
        assert_eq!(text.as_bytes(), value.as_bytes());
        assert!(!format!("{text:?}").contains("中文"));
        let c = TextCommit {
            scope: DrawnScope {
                source_epoch: 1,
                geometry_epoch: 2,
                frame_sequence: 0,
            },
            text,
        };
        assert_eq!(TextCommit::decode(&c.encode().unwrap()).unwrap(), c);
        let mut bad = c.encode().unwrap();
        bad.push(0);
        assert!(TextCommit::decode(&bad).is_err());
    }
    #[test]
    fn bounds_and_malformed_utf8() {
        for b in [vec![], vec![0], vec![255], vec![b'a'; 4001]] {
            assert!(ExplicitText::new(b).is_err());
        }
        assert!(ExplicitText::new(vec![b'a'; 4000]).is_ok());
        assert!(TextResult::decode(&[0; 25]).is_err());
    }
}
