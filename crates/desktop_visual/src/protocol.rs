use std::io;

pub const PROTOCOL_VERSION: u32 = 1;
pub const HEADER_BYTES: usize = 64;
pub const FRAME_MAGIC: [u8; 8] = *b"DVRFRAME";
pub const HELLO_MAGIC: [u8; 8] = *b"DVRHELLO";
pub const MAX_DIMENSION: u32 = 4096;
pub const MAX_FRAME_BYTES: u32 = 32 * 1024 * 1024;

/// Physical-pixel extent of the compositor's complete canvas. Frame dimensions
/// may be smaller: v1 scales the frame over this whole extent, not one output.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OutputSize {
    pub width: u32,
    pub height: u32,
}

impl OutputSize {
    pub(crate) fn validate(self) -> io::Result<()> {
        if self.width == 0 || self.height == 0 || self.width > 65_535 || self.height > 65_535 {
            return Err(invalid("output dimensions must be in 1..=65535"));
        }
        Ok(())
    }

    pub(crate) fn packed(self) -> u64 {
        (u64::from(self.width) << 32) | u64::from(self.height)
    }

    pub(crate) fn unpack(value: u64) -> Self {
        Self {
            width: (value >> 32) as u32,
            height: value as u32,
        }
    }
}

/// RGB is sRGB-encoded. Premultiplication, where selected, is in that encoded
/// domain, matching the consumer's source-over blend contract. Opaque ignores
/// stored alpha; straight is multiplied once by the renderer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum AlphaMode {
    Opaque = 1,
    Straight = 2,
    Premultiplied = 3,
}

impl TryFrom<u32> for AlphaMode {
    type Error = io::Error;

    fn try_from(value: u32) -> io::Result<Self> {
        match value {
            1 => Ok(Self::Opaque),
            2 => Ok(Self::Straight),
            3 => Ok(Self::Premultiplied),
            _ => Err(invalid("unsupported alpha mode")),
        }
    }
}

/// Server capabilities, sent exactly once before frames on each connection.
/// This version deliberately advertises only RGBA8/sRGB/top-left data.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Hello {
    pub output: OutputSize,
}

impl Hello {
    pub fn encode(self) -> io::Result<[u8; HEADER_BYTES]> {
        self.output.validate()?;
        let mut bytes = prefix(HELLO_MAGIC);
        put_u32(&mut bytes, 16, self.output.width);
        put_u32(&mut bytes, 20, self.output.height);
        put_u32(&mut bytes, 24, MAX_DIMENSION);
        put_u32(&mut bytes, 28, MAX_FRAME_BYTES);
        put_u32(&mut bytes, 32, 1); // RGBA8 bit
        put_u32(&mut bytes, 36, 7); // opaque, straight, premultiplied
        put_u32(&mut bytes, 40, 1); // top left
        Ok(bytes)
    }

    pub fn parse(bytes: &[u8]) -> io::Result<Self> {
        validate_prefix(bytes, HELLO_MAGIC)?;
        if read_u32(bytes, 24) != MAX_DIMENSION
            || read_u32(bytes, 28) != MAX_FRAME_BYTES
            || read_u32(bytes, 32) != 1
            || read_u32(bytes, 36) != 7
            || read_u32(bytes, 40) != 1
            || bytes[44..64].iter().any(|byte| *byte != 0)
        {
            return Err(invalid("unsupported desktop visual capabilities"));
        }
        let output = OutputSize {
            width: read_u32(bytes, 16),
            height: read_u32(bytes, 20),
        };
        output.validate()?;
        Ok(Self { output })
    }
}

/// Validated source metadata. `stride` describes the wire payload; the server's
/// `Frame::rgba` is compacted to `width * height * 4` bytes before publication.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Header {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub alpha: AlphaMode,
    pub sequence: u64,
    pub timestamp_ns: u64,
}

impl Header {
    pub fn encode(self) -> io::Result<[u8; HEADER_BYTES]> {
        self.validate()?;
        let mut bytes = prefix(FRAME_MAGIC);
        put_u32(&mut bytes, 16, self.width);
        put_u32(&mut bytes, 20, self.height);
        put_u32(&mut bytes, 24, self.stride);
        put_u32(&mut bytes, 28, 1); // RGBA8
        put_u32(&mut bytes, 32, 1); // sRGB
        put_u32(&mut bytes, 36, self.alpha as u32);
        put_u32(&mut bytes, 40, 1); // top left
        bytes[48..56].copy_from_slice(&self.sequence.to_le_bytes());
        bytes[56..64].copy_from_slice(&self.timestamp_ns.to_le_bytes());
        Ok(bytes)
    }

    pub fn parse(bytes: &[u8]) -> io::Result<Self> {
        validate_prefix(bytes, FRAME_MAGIC)?;
        if read_u32(bytes, 28) != 1
            || read_u32(bytes, 32) != 1
            || read_u32(bytes, 40) != 1
            || read_u32(bytes, 44) != 0
        {
            return Err(invalid("unsupported pixel/color/origin/reserved field"));
        }
        let header = Self {
            width: read_u32(bytes, 16),
            height: read_u32(bytes, 20),
            stride: read_u32(bytes, 24),
            alpha: AlphaMode::try_from(read_u32(bytes, 36))?,
            sequence: u64::from_le_bytes(bytes[48..56].try_into().unwrap()),
            timestamp_ns: u64::from_le_bytes(bytes[56..64].try_into().unwrap()),
        };
        header.validate()?;
        Ok(header)
    }

    /// Checked even for headers constructed directly by a caller.
    pub fn payload_len(self) -> io::Result<usize> {
        self.validate()?;
        Ok((u64::from(self.stride) * u64::from(self.height)) as usize)
    }

    fn validate(self) -> io::Result<()> {
        if self.width == 0
            || self.height == 0
            || self.width > MAX_DIMENSION
            || self.height > MAX_DIMENSION
        {
            return Err(invalid("frame dimensions exceed negotiated limits"));
        }
        if self.stride < self.width * 4 {
            return Err(invalid("stride is smaller than a pixel row"));
        }
        if u64::from(self.stride) * u64::from(self.height) > u64::from(MAX_FRAME_BYTES) {
            return Err(invalid("frame payload exceeds negotiated limit"));
        }
        if self.sequence == 0 {
            return Err(invalid("frame sequence must be nonzero"));
        }
        Ok(())
    }
}

fn prefix(magic: [u8; 8]) -> [u8; HEADER_BYTES] {
    let mut bytes = [0; HEADER_BYTES];
    bytes[..8].copy_from_slice(&magic);
    put_u32(&mut bytes, 8, PROTOCOL_VERSION);
    put_u32(&mut bytes, 12, HEADER_BYTES as u32);
    bytes
}

fn validate_prefix(bytes: &[u8], magic: [u8; 8]) -> io::Result<()> {
    if bytes.len() != HEADER_BYTES {
        return Err(invalid("record header must be exactly 64 bytes"));
    }
    if bytes[..8] != magic
        || read_u32(bytes, 8) != PROTOCOL_VERSION
        || read_u32(bytes, 12) != HEADER_BYTES as u32
    {
        return Err(invalid(
            "unsupported desktop visual magic/version/header size",
        ));
    }
    Ok(())
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

pub(crate) fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> Header {
        Header {
            width: 3,
            height: 2,
            stride: 16,
            alpha: AlphaMode::Straight,
            sequence: 7,
            timestamp_ns: 123_456,
        }
    }

    #[test]
    fn header_roundtrip_and_little_endian_contract() {
        let bytes = header().encode().unwrap();
        assert_eq!(&bytes[..8], b"DVRFRAME");
        assert_eq!(&bytes[48..56], &7_u64.to_le_bytes());
        assert_eq!(Header::parse(&bytes).unwrap(), header());
        assert_eq!(header().payload_len().unwrap(), 32);
        for alpha in [
            AlphaMode::Opaque,
            AlphaMode::Straight,
            AlphaMode::Premultiplied,
        ] {
            let source = Header { alpha, ..header() };
            assert_eq!(Header::parse(&source.encode().unwrap()).unwrap(), source);
        }
    }

    #[test]
    fn malformed_headers_are_rejected_before_allocation() {
        for (offset, value) in [
            (8, 0),
            (8, 2),
            (12, 63),
            (16, 0),
            (16, 4097),
            (20, 0),
            (20, 4097),
            (24, 11),
            (24, u32::MAX),
            (28, 0),
            (32, 2),
            (36, 0),
            (36, 4),
            (40, 2),
            (44, 1),
        ] {
            let mut bytes = header().encode().unwrap();
            put_u32(&mut bytes, offset, value);
            assert!(
                Header::parse(&bytes).is_err(),
                "offset {offset}, value {value}"
            );
        }
        let mut bytes = header().encode().unwrap();
        bytes[0] ^= 1;
        assert!(Header::parse(&bytes).is_err());
        let mut bytes = header().encode().unwrap();
        bytes[48..56].fill(0);
        assert!(Header::parse(&bytes).is_err());
        for length in 0..HEADER_BYTES {
            assert!(Header::parse(&bytes[..length]).is_err());
        }
        assert!(Header::parse(&[0; HEADER_BYTES + 1]).is_err());
    }

    #[test]
    fn byte_limit_includes_padding_and_is_inclusive() {
        let exact = Header {
            width: 4096,
            height: 2048,
            stride: 16384,
            ..header()
        };
        assert_eq!(exact.payload_len().unwrap(), MAX_FRAME_BYTES as usize);
        assert!(
            Header {
                stride: 16385,
                ..exact
            }
            .encode()
            .is_err()
        );
        assert!(
            Header {
                height: 4096,
                ..exact
            }
            .encode()
            .is_err()
        );
    }

    #[test]
    fn hello_roundtrip_and_reserved_fields() {
        let hello = Hello {
            output: OutputSize {
                width: 7680,
                height: 2160,
            },
        };
        let bytes = hello.encode().unwrap();
        assert_eq!(Hello::parse(&bytes).unwrap(), hello);
        assert_eq!(&bytes[28..32], &MAX_FRAME_BYTES.to_le_bytes());
        for offset in [8, 12, 16, 20, 24, 28, 32, 36, 40] {
            let mut bad = bytes;
            put_u32(&mut bad, offset, 0);
            assert!(Hello::parse(&bad).is_err());
        }
        for offset in 44..64 {
            let mut bad = bytes;
            bad[offset] = 1;
            assert!(Hello::parse(&bad).is_err());
        }
    }

    #[test]
    fn python_sdk_golden_frame_bytes_match_exactly() {
        let header = Header {
            width: 2,
            height: 1,
            stride: 8,
            alpha: AlphaMode::Straight,
            sequence: 0x0102030405060708,
            timestamp_ns: 0x1112131415161718,
        };
        let hex = "4456524652414d450100000040000000020000000100000008000000010000000100000002000000010000000000000008070605040302011817161514131211";
        let expected: Vec<u8> = hex
            .as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect();
        assert_eq!(header.encode().unwrap().as_slice(), expected);
        assert_eq!(Header::parse(&expected).unwrap(), header);
    }
}
