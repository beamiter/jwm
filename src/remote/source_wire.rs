//! Bounded source identity messages. No peer-supplied capture geometry crosses
//! this boundary: IDs only refer to the immutable host grant catalog.
use super::RemoteResult;
use super::protocol::MAX_PAYLOAD_LEN;
use super::source_session::{MAX_GRANTED_SOURCES, SourceId, SourceVersion};
use std::io;

pub const MAX_SOURCE_LABEL_LEN: usize = 256;
pub const SOURCE_EPOCH_PREFIX_LEN: usize = 8;
/// Maximum inner frame/cursor bytes, reserving source epoch and padding marker.
pub const MAX_SOURCE_INNER_PAYLOAD_LEN: usize = MAX_PAYLOAD_LEN - SOURCE_EPOCH_PREFIX_LEN - 1;
/// Application v26 frame records carry an authenticated, immutable purpose.
pub const FRAME_PREFIX_LEN: usize = SOURCE_EPOCH_PREFIX_LEN + 1;
pub const MAX_FRAME_INNER_PAYLOAD_LEN: usize = MAX_PAYLOAD_LEN - FRAME_PREFIX_LEN - 1;
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum FramePurpose {
    #[default]
    Video = 0,
    NativeProof = 1,
    VisualRepair = 2,
    PopupFamilyProof = 3,
}

pub fn encode_frame_payload(
    epoch: u64,
    purpose: FramePurpose,
    inner: &[u8],
) -> RemoteResult<Vec<u8>> {
    if epoch == 0 || inner.is_empty() || inner.len() > MAX_FRAME_INNER_PAYLOAD_LEN {
        return Err(invalid("invalid frame envelope epoch or length").into());
    }
    let mut bytes = Vec::with_capacity(FRAME_PREFIX_LEN + inner.len());
    bytes.extend_from_slice(&epoch.to_be_bytes());
    bytes.push(purpose as u8);
    bytes.extend_from_slice(inner);
    Ok(bytes)
}
pub fn decode_frame_payload(bytes: &[u8]) -> RemoteResult<(u64, FramePurpose, &[u8])> {
    if bytes.len() <= FRAME_PREFIX_LEN || bytes.len() >= MAX_PAYLOAD_LEN {
        return Err(invalid("invalid frame envelope length").into());
    }
    let epoch = u64::from_be_bytes(bytes[..8].try_into().unwrap());
    if epoch == 0 {
        return Err(invalid("zero frame source epoch").into());
    }
    let purpose = match bytes[8] {
        0 => FramePurpose::Video,
        1 => FramePurpose::NativeProof,
        2 => FramePurpose::VisualRepair,
        3 => FramePurpose::PopupFamilyProof,
        _ => return Err(invalid("unknown frame purpose").into()),
    };
    Ok((epoch, purpose, &bytes[FRAME_PREFIX_LEN..]))
}

pub const MAX_SOURCE_CATALOG_LEN: usize = 13 + MAX_GRANTED_SOURCES * (6 + MAX_SOURCE_LABEL_LEN);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceEntry {
    pub id: SourceId,
    pub label: String,
}

/// A connection sends this once; subsequent changes only select an entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceCatalog {
    pub current: SourceVersion,
    pub entries: Vec<SourceEntry>,
}
impl SourceCatalog {
    pub fn validate(&self) -> RemoteResult<()> {
        validate_version(self.current)?;
        if self.entries.is_empty() || self.entries.len() > MAX_GRANTED_SOURCES {
            return Err(invalid("invalid source catalog count").into());
        }
        for (index, entry) in self.entries.iter().enumerate() {
            if entry.id.0 == 0
                || entry.label.is_empty()
                || entry.label.len() > MAX_SOURCE_LABEL_LEN
                || entry.label.chars().any(char::is_control)
                || self.entries[..index]
                    .iter()
                    .any(|previous| previous.id == entry.id)
            {
                return Err(invalid("invalid source catalog entry").into());
            }
        }
        if !self
            .entries
            .iter()
            .any(|entry| entry.id == self.current.source)
        {
            return Err(invalid("current source is absent from catalog").into());
        }
        Ok(())
    }

    pub fn encode(&self) -> RemoteResult<Vec<u8>> {
        self.validate()?;
        let mut payload = Vec::new();
        payload.extend_from_slice(&encode_source_changed(self.current)?);
        payload.push(self.entries.len() as u8);
        for entry in &self.entries {
            payload.extend_from_slice(&entry.id.0.to_be_bytes());
            payload.extend_from_slice(&(entry.label.len() as u16).to_be_bytes());
            payload.extend_from_slice(entry.label.as_bytes());
        }
        Ok(payload)
    }

    pub fn decode(payload: &[u8]) -> RemoteResult<Self> {
        if payload.len() < 13 || payload.len() > MAX_SOURCE_CATALOG_LEN {
            return Err(invalid("invalid source catalog length").into());
        }
        let current = decode_source_changed(&payload[..12])?;
        let count = usize::from(payload[12]);
        if count == 0 || count > MAX_GRANTED_SOURCES {
            return Err(invalid("invalid source catalog count").into());
        }
        let mut rest = &payload[13..];
        let mut entries = Vec::with_capacity(count);
        for _ in 0..count {
            if rest.len() < 6 {
                return Err(invalid("truncated source catalog entry").into());
            }
            let id = SourceId(u32::from_be_bytes(rest[..4].try_into().unwrap()));
            let len = usize::from(u16::from_be_bytes(rest[4..6].try_into().unwrap()));
            rest = &rest[6..];
            if len == 0 || len > MAX_SOURCE_LABEL_LEN || rest.len() < len {
                return Err(invalid("invalid source label length").into());
            }
            let label = std::str::from_utf8(&rest[..len])?.to_owned();
            entries.push(SourceEntry { id, label });
            rest = &rest[len..];
        }
        if !rest.is_empty() {
            return Err(invalid("trailing source catalog bytes").into());
        }
        let catalog = Self { current, entries };
        catalog.validate()?;
        Ok(catalog)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceRequest {
    pub expected_epoch: u64,
    pub target: SourceId,
}
impl SourceRequest {
    pub fn encode(self) -> RemoteResult<[u8; 12]> {
        encode_source_changed(SourceVersion {
            epoch: self.expected_epoch,
            source: self.target,
        })
    }
    pub fn decode(payload: &[u8]) -> RemoteResult<Self> {
        let version = decode_source_changed(payload)?;
        Ok(Self {
            expected_epoch: version.epoch,
            target: version.source,
        })
    }
}

pub fn encode_source_changed(version: SourceVersion) -> RemoteResult<[u8; 12]> {
    validate_version(version)?;
    let mut payload = [0; 12];
    payload[..8].copy_from_slice(&version.epoch.to_be_bytes());
    payload[8..].copy_from_slice(&version.source.0.to_be_bytes());
    Ok(payload)
}
pub fn decode_source_changed(payload: &[u8]) -> RemoteResult<SourceVersion> {
    if payload.len() != 12 {
        return Err(invalid("invalid source version length").into());
    }
    let version = SourceVersion {
        epoch: u64::from_be_bytes(payload[..8].try_into().unwrap()),
        source: SourceId(u32::from_be_bytes(payload[8..].try_into().unwrap())),
    };
    validate_version(version)?;
    Ok(version)
}

/// Envelope for CursorShape and CursorPosition; frames use encode_frame_payload. The transport also
/// needs its padding marker, hence the one-byte reservation below the cap.
pub fn encode_source_payload(epoch: u64, inner: &[u8]) -> RemoteResult<Vec<u8>> {
    if epoch == 0 || inner.is_empty() || inner.len() > MAX_SOURCE_INNER_PAYLOAD_LEN {
        return Err(invalid("invalid source payload epoch or length").into());
    }
    let mut payload = Vec::with_capacity(SOURCE_EPOCH_PREFIX_LEN + inner.len());
    payload.extend_from_slice(&epoch.to_be_bytes());
    payload.extend_from_slice(inner);
    Ok(payload)
}
pub fn decode_source_payload(payload: &[u8]) -> RemoteResult<(u64, &[u8])> {
    if payload.len() <= SOURCE_EPOCH_PREFIX_LEN || payload.len() >= MAX_PAYLOAD_LEN {
        return Err(invalid("invalid source payload length").into());
    }
    let epoch = u64::from_be_bytes(payload[..8].try_into().unwrap());
    if epoch == 0 {
        return Err(invalid("zero source epoch").into());
    }
    Ok((epoch, &payload[8..]))
}
fn validate_version(version: SourceVersion) -> RemoteResult<()> {
    if version.epoch == 0 || version.source.0 == 0 {
        return Err(invalid("zero source epoch or identifier").into());
    }
    Ok(())
}
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn catalog() -> SourceCatalog {
        SourceCatalog {
            current: SourceVersion {
                epoch: 1,
                source: SourceId(1),
            },
            entries: vec![
                SourceEntry {
                    id: SourceId(1),
                    label: "Monitor 🖥".into(),
                },
                SourceEntry {
                    id: SourceId(2),
                    label: "Region".into(),
                },
            ],
        }
    }
    #[test]
    fn malformed_catalog_bytes_fail_closed() {
        let mut value = catalog();
        value.entries[0].label = "x".into();
        let payload = value.encode().unwrap();
        // First entry starts at 13; its one-byte label ends at 20.
        for (offset, bytes) in [(13, 0_u32.to_be_bytes()), (20, 1_u32.to_be_bytes())] {
            let mut bad = payload.clone();
            bad[offset..offset + 4].copy_from_slice(&bytes);
            assert!(SourceCatalog::decode(&bad).is_err());
        }
        for len in [0_u16, 257, u16::MAX] {
            let mut bad = payload.clone();
            bad[17..19].copy_from_slice(&len.to_be_bytes());
            assert!(SourceCatalog::decode(&bad).is_err());
        }
        for byte in [0, b'\n', 0xff] {
            let mut bad = payload.clone();
            bad[19] = byte;
            assert!(SourceCatalog::decode(&bad).is_err());
        }
        for range in [0..8, 8..12] {
            let mut bad = payload.clone();
            bad[range].fill(0);
            assert!(SourceCatalog::decode(&bad).is_err());
        }
    }

    #[test]
    fn catalog_roundtrip_and_all_truncations() {
        let expected = catalog();
        let payload = expected.encode().unwrap();
        assert_eq!(SourceCatalog::decode(&payload).unwrap(), expected);
        for len in 0..payload.len() {
            assert!(SourceCatalog::decode(&payload[..len]).is_err());
        }
        let mut trailing = payload;
        trailing.push(0);
        assert!(SourceCatalog::decode(&trailing).is_err());
    }
    #[test]
    fn invalid_catalog_fields_are_rejected() {
        for label in [
            "".to_string(),
            "a".repeat(257),
            "bad\nlabel".into(),
            "bad\u{0085}label".into(),
        ] {
            let mut value = catalog();
            value.entries[0].label = label;
            assert!(value.encode().is_err());
        }
        for id in [0, 2] {
            let mut value = catalog();
            value.entries[0].id = SourceId(id);
            assert!(value.encode().is_err());
        }
        let mut value = catalog();
        value.current.source = SourceId(3);
        assert!(value.encode().is_err());
        value = catalog();
        value.current.epoch = 0;
        assert!(value.encode().is_err());
        let mut payload = catalog().encode().unwrap();
        payload[19] = 0xff;
        assert!(SourceCatalog::decode(&payload).is_err());
        assert!(SourceCatalog::decode(&vec![0; MAX_SOURCE_CATALOG_LEN + 1]).is_err());
    }
    #[test]
    fn catalog_maximum_and_forged_count() {
        let mut value = catalog();
        value.entries = (1..=32)
            .map(|id| SourceEntry {
                id: SourceId(id),
                label: "a".repeat(256),
            })
            .collect();
        let mut payload = value.encode().unwrap();
        assert_eq!(payload.len(), MAX_SOURCE_CATALOG_LEN);
        assert_eq!(SourceCatalog::decode(&payload).unwrap(), value);
        for count in [0, 33, 255] {
            payload[12] = count;
            assert!(SourceCatalog::decode(&payload).is_err());
        }
        value.entries.push(SourceEntry {
            id: SourceId(33),
            label: "x".into(),
        });
        assert!(value.encode().is_err());
    }
    #[test]
    fn fixed_messages_and_envelope_are_strict() {
        let request = SourceRequest {
            expected_epoch: u64::MAX,
            target: SourceId(u32::MAX),
        };
        let payload = request.encode().unwrap();
        assert_eq!(SourceRequest::decode(&payload).unwrap(), request);
        for len in 0..12 {
            assert!(SourceRequest::decode(&payload[..len]).is_err());
        }
        assert!(SourceRequest::decode(&[0; 13]).is_err());
        assert!(SourceRequest::decode(&[0; 12]).is_err());
        let payload = encode_source_payload(7, b"data").unwrap();
        assert_eq!(
            decode_source_payload(&payload).unwrap(),
            (7, b"data".as_slice())
        );
        for len in 0..=8 {
            assert!(decode_source_payload(&payload[..len]).is_err());
        }
        assert!(encode_source_payload(0, b"data").is_err());
        assert!(decode_source_payload(&[0; 9]).is_err());
    }
}

#[cfg(test)]
mod frame_purpose_tests {
    use super::*;
    #[test]
    fn authenticated_frame_purpose_is_explicit_and_strict() {
        for purpose in [
            FramePurpose::Video,
            FramePurpose::NativeProof,
            FramePurpose::VisualRepair,
            FramePurpose::PopupFamilyProof,
        ] {
            let bytes = encode_frame_payload(7, purpose, b"pixels").unwrap();
            assert_eq!(
                decode_frame_payload(&bytes).unwrap(),
                (7, purpose, &b"pixels"[..])
            );
            for n in 0..=FRAME_PREFIX_LEN {
                assert!(decode_frame_payload(&bytes[..n]).is_err());
            }
            for bad in [4, 127, 255] {
                let mut b = bytes.clone();
                b[8] = bad;
                assert!(decode_frame_payload(&b).is_err());
            }
        }
        assert!(encode_frame_payload(0, FramePurpose::Video, b"x").is_err());
        assert!(encode_frame_payload(1, FramePurpose::Video, b"").is_err());
        assert!(decode_frame_payload(&[0; 10]).is_err());
        assert!(decode_frame_payload(&vec![1; MAX_PAYLOAD_LEN]).is_err());
        let max = vec![7; MAX_FRAME_INNER_PAYLOAD_LEN];
        let encoded = encode_frame_payload(1, FramePurpose::Video, &max).unwrap();
        assert_eq!(encoded.len() + 1, MAX_PAYLOAD_LEN); // authenticated padding marker
        assert_eq!(decode_frame_payload(&encoded).unwrap().2.len(), max.len());
        assert!(
            encode_frame_payload(
                1,
                FramePurpose::Video,
                &vec![7; MAX_FRAME_INNER_PAYLOAD_LEN + 1]
            )
            .is_err()
        );
        assert_eq!(
            MAX_SOURCE_INNER_PAYLOAD_LEN,
            MAX_FRAME_INNER_PAYLOAD_LEN + 1
        );
    }
}

#[cfg(test)]
mod popup_envelope_boundary_tests {
    use super::*;
    #[test]
    fn every_purpose_tag_and_exact_frame_budget_is_checked_before_slicing() {
        let mut bytes =
            encode_frame_payload(u64::MAX, FramePurpose::PopupFamilyProof, &[42]).unwrap();
        for tag in 0..=u8::MAX {
            bytes[8] = tag;
            let decoded = decode_frame_payload(&bytes);
            assert_eq!(decoded.is_ok(), tag <= 3, "purpose {tag}");
            if let Ok((epoch, purpose, inner)) = decoded {
                assert_eq!(epoch, u64::MAX);
                assert_eq!(inner, [42]);
                assert_eq!(encode_frame_payload(epoch, purpose, inner).unwrap(), bytes);
            }
        }
        bytes[8] = 3;
        for length in 0..=FRAME_PREFIX_LEN {
            assert!(decode_frame_payload(&bytes[..length]).is_err());
        }
        bytes[..8].fill(0);
        assert!(decode_frame_payload(&bytes).is_err());
        let mut boundary = vec![0; MAX_PAYLOAD_LEN + 1];
        boundary[..8].copy_from_slice(&1u64.to_be_bytes());
        boundary[8] = 3;
        assert_eq!(
            decode_frame_payload(&boundary[..MAX_PAYLOAD_LEN - 1])
                .unwrap()
                .2
                .len(),
            MAX_FRAME_INNER_PAYLOAD_LEN
        );
        assert!(decode_frame_payload(&boundary[..MAX_PAYLOAD_LEN]).is_err());
        assert!(decode_frame_payload(&boundary).is_err());
        assert!(
            encode_frame_payload(
                1,
                FramePurpose::PopupFamilyProof,
                &boundary[..MAX_FRAME_INNER_PAYLOAD_LEN + 1]
            )
            .is_err()
        );
        // Unlike fixed metadata, the envelope intentionally owns a variable
        // inner payload. Its trailing bytes are not a canonicality violation.
    }
}
