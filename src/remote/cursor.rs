//! Bounded independent cursor plane. Pixels are premultiplied RGBA; positions
//! live in shared-source coordinates and are bound to a video geometry epoch.
use super::RemoteResult;
use std::{io, sync::Arc};

pub const MAX_CURSOR_DIMENSION: u16 = 256;
const SHAPE_HEADER: usize = 16;
const POSITION_LEN: usize = 37;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CursorShape {
    pub id: u64,
    pub width: u16,
    pub height: u16,
    pub hotspot_x: u16,
    pub hotspot_y: u16,
    pub rgba: Vec<u8>,
}
impl CursorShape {
    pub fn validate(&self) -> RemoteResult<()> {
        if self.id == 0
            || self.width == 0
            || self.height == 0
            || self.width > MAX_CURSOR_DIMENSION
            || self.height > MAX_CURSOR_DIMENSION
            || self.hotspot_x >= self.width
            || self.hotspot_y >= self.height
            || self.rgba.len() != usize::from(self.width) * usize::from(self.height) * 4
            || self
                .rgba
                .chunks_exact(4)
                .any(|p| p[..3].iter().any(|v| *v > p[3]))
        {
            return Err(invalid(
                "invalid cursor shape dimensions, hotspot or premultiplied pixels",
            ));
        }
        Ok(())
    }
    pub fn encode(&self) -> RemoteResult<Vec<u8>> {
        self.validate()?;
        let mut output = Vec::with_capacity(SHAPE_HEADER + self.rgba.len());
        output.extend_from_slice(&self.id.to_be_bytes());
        for n in [self.width, self.height, self.hotspot_x, self.hotspot_y] {
            output.extend_from_slice(&n.to_be_bytes());
        }
        output.extend_from_slice(&self.rgba);
        Ok(output)
    }
    pub fn decode(payload: &[u8]) -> RemoteResult<Self> {
        if payload.len() < SHAPE_HEADER
            || payload.len() > SHAPE_HEADER + usize::from(MAX_CURSOR_DIMENSION).pow(2) * 4
        {
            return Err(invalid("invalid cursor shape length"));
        }
        let shape = Self {
            id: u64::from_be_bytes(payload[..8].try_into().unwrap()),
            width: u16::from_be_bytes(payload[8..10].try_into().unwrap()),
            height: u16::from_be_bytes(payload[10..12].try_into().unwrap()),
            hotspot_x: u16::from_be_bytes(payload[12..14].try_into().unwrap()),
            hotspot_y: u16::from_be_bytes(payload[14..16].try_into().unwrap()),
            rgba: payload[16..].to_vec(),
        };
        shape.validate()?;
        Ok(shape)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CursorPosition {
    pub sequence: u64,
    pub geometry_epoch: u64,
    pub shape_id: u64,
    pub source_width: u16,
    pub source_height: u16,
    pub x: i32,
    pub y: i32,
    pub visible: bool,
}
impl CursorPosition {
    pub fn encode(self) -> RemoteResult<Vec<u8>> {
        self.validate()?;
        let mut output = Vec::with_capacity(POSITION_LEN);
        for n in [self.sequence, self.geometry_epoch, self.shape_id] {
            output.extend_from_slice(&n.to_be_bytes());
        }
        output.extend_from_slice(&self.source_width.to_be_bytes());
        output.extend_from_slice(&self.source_height.to_be_bytes());
        output.extend_from_slice(&self.x.to_be_bytes());
        output.extend_from_slice(&self.y.to_be_bytes());
        output.push(u8::from(self.visible));
        Ok(output)
    }
    fn validate(self) -> RemoteResult<()> {
        super::frame::validate_dimensions(self.source_width, self.source_height)?;
        if self.geometry_epoch == 0
            || self.shape_id == 0
            || (self.visible
                && (self.x < 0
                    || self.y < 0
                    || self.x >= i32::from(self.source_width)
                    || self.y >= i32::from(self.source_height)))
        {
            return Err(invalid("invalid cursor position or geometry identity"));
        }
        Ok(())
    }
    pub fn decode(payload: &[u8]) -> RemoteResult<Self> {
        if payload.len() != POSITION_LEN || payload[36] > 1 {
            return Err(invalid("invalid cursor position length or visibility"));
        }
        let position = Self {
            sequence: u64::from_be_bytes(payload[..8].try_into().unwrap()),
            geometry_epoch: u64::from_be_bytes(payload[8..16].try_into().unwrap()),
            shape_id: u64::from_be_bytes(payload[16..24].try_into().unwrap()),
            source_width: u16::from_be_bytes(payload[24..26].try_into().unwrap()),
            source_height: u16::from_be_bytes(payload[26..28].try_into().unwrap()),
            x: i32::from_be_bytes(payload[28..32].try_into().unwrap()),
            y: i32::from_be_bytes(payload[32..36].try_into().unwrap()),
            visible: payload[36] != 0,
        };
        position.validate()?;
        Ok(position)
    }
}

/// At most the current shape plus one pending update; motion never queues.
#[derive(Default)]
pub(crate) struct CursorInbox {
    shape: Option<Arc<CursorShape>>,
    pending: Option<(Arc<CursorShape>, CursorPosition)>,
    last_sequence: Option<u64>,
    last_epoch: u64,
}
impl CursorInbox {
    pub(crate) fn shape(&mut self, shape: CursorShape) -> RemoteResult<()> {
        shape.validate()?;
        if let Some(current) = self.shape.as_ref()
            && current.id == shape.id
            && **current != shape
        {
            return Err(invalid("cursor shape id was reused with different pixels"));
        }
        self.shape = Some(Arc::new(shape));
        Ok(())
    }
    pub(crate) fn position(&mut self, position: CursorPosition) -> RemoteResult<()> {
        position.validate()?;
        if self
            .last_sequence
            .is_some_and(|last| position.sequence <= last)
            || position.geometry_epoch < self.last_epoch
        {
            return Err(invalid(
                "cursor position sequence or geometry moved backwards",
            ));
        }
        let shape = self
            .shape
            .as_ref()
            .filter(|s| s.id == position.shape_id)
            .ok_or_else(|| invalid("cursor position references an unknown shape"))?;
        self.pending = Some((Arc::clone(shape), position));
        self.last_sequence = Some(position.sequence);
        self.last_epoch = position.geometry_epoch;
        Ok(())
    }
    pub(crate) fn take(&mut self) -> Option<(Arc<CursorShape>, CursorPosition)> {
        self.pending.take()
    }
}
fn invalid(message: &str) -> super::RemoteError {
    io::Error::new(io::ErrorKind::InvalidData, message).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn shape() -> CursorShape {
        CursorShape {
            id: 1,
            width: 1,
            height: 1,
            hotspot_x: 0,
            hotspot_y: 0,
            rgba: vec![20, 30, 40, 255],
        }
    }
    fn position() -> CursorPosition {
        CursorPosition {
            sequence: 0,
            geometry_epoch: 1,
            shape_id: 1,
            source_width: 640,
            source_height: 480,
            x: 20,
            y: 30,
            visible: true,
        }
    }
    #[test]
    fn cursor_wire_is_bounded_and_strict() {
        let s = shape();
        assert_eq!(CursorShape::decode(&s.encode().unwrap()).unwrap(), s);
        let p = position();
        assert_eq!(CursorPosition::decode(&p.encode().unwrap()).unwrap(), p);
        for n in 0..POSITION_LEN {
            assert!(CursorPosition::decode(&p.encode().unwrap()[..n]).is_err());
        }
        let mut malformed = s.clone();
        malformed.hotspot_x = 1;
        assert!(malformed.encode().is_err());
        malformed = s.clone();
        malformed.rgba[3] = 0;
        assert!(malformed.encode().is_err());
        malformed = s;
        malformed.width = 257;
        assert!(malformed.encode().is_err());
        let mut invisible = p;
        invisible.visible = false;
        invisible.x = -100;
        assert!(invisible.encode().is_ok());
        invisible.visible = true;
        assert!(invisible.encode().is_err());
    }
    #[test]
    fn latest_position_is_bounded_and_unknown_shapes_and_replays_fail() {
        let mut inbox = CursorInbox::default();
        assert!(inbox.position(position()).is_err());
        inbox.shape(shape()).unwrap();
        for sequence in 0..1000 {
            inbox
                .position(CursorPosition {
                    sequence,
                    ..position()
                })
                .unwrap();
        }
        let (_, p) = inbox.take().unwrap();
        assert_eq!(p.sequence, 999);
        assert!(inbox.take().is_none());
        assert!(inbox.position(p).is_err());
        assert!(
            inbox
                .position(CursorPosition {
                    sequence: 1000,
                    shape_id: 2,
                    ..p
                })
                .is_err()
        );
    }
}
