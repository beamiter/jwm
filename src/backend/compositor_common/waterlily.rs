//! Backend-neutral WaterLily frame protocol and shared-file reader.
//!
//! The Julia worker publishes only completed frames. A tiny Unix-stream message
//! wakes the compositor, while the pixels live in a private double-buffer file.
//! Keeping the transport independent from GL/Smithay lets Wayland consume the
//! same protocol in a later iteration.

use std::fs::{File, Metadata, OpenOptions};
use std::io;
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

pub const WATERLILY_MAGIC: [u8; 8] = *b"JWMLILY\0";
pub const WATERLILY_PROTOCOL_VERSION: u32 = 1;
/// Version 2 stacks `depth` two-dimensional slices per slot, front (nearest
/// the viewer) to back, so a planar frame is exactly a volume of depth one.
pub const WATERLILY_PROTOCOL_VERSION_VOLUMETRIC: u32 = 2;
pub const WATERLILY_HEADER_BYTES: usize = 64;
/// The volumetric header keeps the version-1 prefix byte-for-byte and appends
/// the depth plus reserved space, so both versions parse from one prefix.
pub const WATERLILY_VOLUME_HEADER_BYTES: usize = 96;
pub const WATERLILY_PIXEL_FORMAT_RGBA8: u32 = 1;
pub const WATERLILY_COLOR_SPACE_SRGB: u32 = 1;
pub const WATERLILY_ALPHA_OPAQUE: u32 = 1;
pub const WATERLILY_ORIGIN_TOP_LEFT: u32 = 1;

const SLOT_COUNT: u64 = 2;
const MAX_DIMENSION: u32 = 16_384;
const MAX_FRAME_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WaterlilyFrameHeader {
    pub width: u32,
    pub height: u32,
    /// Depth slices per slot; one for planar version-1 frames.
    pub depth: u32,
    pub stride: u32,
    pub slot: u32,
    pub sequence: u64,
    pub timestamp_ns: u64,
    /// Bytes the header occupies in the file; slots start right behind it.
    pub header_len: u32,
}

impl WaterlilyFrameHeader {
    /// Parse a header from the file prefix. `bytes` must hold at least the
    /// version-1 header; volumetric headers consume their extension from the
    /// same slice.
    pub fn parse(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() < WATERLILY_HEADER_BYTES {
            return Err(invalid_data("truncated WaterLily frame header"));
        }
        if bytes[..8] != WATERLILY_MAGIC {
            return Err(invalid_data("invalid WaterLily frame magic"));
        }
        let version = read_u32(bytes, 8);
        let header_len = read_u32(bytes, 12);
        let (expected_header, depth) = match version {
            WATERLILY_PROTOCOL_VERSION => (WATERLILY_HEADER_BYTES, 1),
            WATERLILY_PROTOCOL_VERSION_VOLUMETRIC => {
                if bytes.len() < WATERLILY_VOLUME_HEADER_BYTES {
                    return Err(invalid_data("truncated WaterLily volumetric header"));
                }
                (WATERLILY_VOLUME_HEADER_BYTES, read_u32(bytes, 64))
            }
            _ => return Err(invalid_data("unsupported WaterLily protocol version")),
        };
        if header_len as usize != expected_header {
            return Err(invalid_data("invalid WaterLily header length"));
        }

        let width = read_u32(bytes, 16);
        let height = read_u32(bytes, 20);
        let stride = read_u32(bytes, 24);
        let pixel_format = read_u32(bytes, 28);
        let color_space = read_u32(bytes, 32);
        let alpha_mode = read_u32(bytes, 36);
        let origin = read_u32(bytes, 40);
        let slot = read_u32(bytes, 44);
        let sequence = read_u64(bytes, 48);
        let timestamp_ns = read_u64(bytes, 56);

        if width == 0 || height == 0 || width > MAX_DIMENSION || height > MAX_DIMENSION {
            return Err(invalid_data("WaterLily frame dimensions are out of range"));
        }
        if depth == 0 || depth > MAX_DIMENSION {
            return Err(invalid_data("WaterLily frame depth is out of range"));
        }
        let tight_stride = width
            .checked_mul(4)
            .ok_or_else(|| invalid_data("WaterLily row size overflow"))?;
        if stride < tight_stride {
            return Err(invalid_data(
                "WaterLily stride is smaller than one RGBA row",
            ));
        }
        if pixel_format != WATERLILY_PIXEL_FORMAT_RGBA8
            || color_space != WATERLILY_COLOR_SPACE_SRGB
            || alpha_mode != WATERLILY_ALPHA_OPAQUE
            || origin != WATERLILY_ORIGIN_TOP_LEFT
        {
            return Err(invalid_data(
                "unsupported WaterLily pixel/color/alpha/origin contract",
            ));
        }
        if slot as u64 >= SLOT_COUNT {
            return Err(invalid_data("invalid WaterLily frame slot"));
        }
        if sequence == 0 {
            return Err(invalid_data("WaterLily frame sequence must be non-zero"));
        }

        let slot_bytes = u64::from(stride)
            .checked_mul(u64::from(height))
            .and_then(|plane| plane.checked_mul(u64::from(depth)))
            .ok_or_else(|| invalid_data("WaterLily slot size overflow"))?;
        if slot_bytes > MAX_FRAME_BYTES {
            return Err(invalid_data("WaterLily frame exceeds the transport limit"));
        }

        Ok(Self {
            width,
            height,
            depth,
            stride,
            slot,
            sequence,
            timestamp_ns,
            header_len,
        })
    }

    fn slot_bytes(self) -> u64 {
        u64::from(self.stride) * u64::from(self.height) * u64::from(self.depth)
    }

    fn slot_offset(self) -> io::Result<u64> {
        u64::from(self.header_len)
            .checked_add(
                u64::from(self.slot)
                    .checked_mul(self.slot_bytes())
                    .ok_or_else(|| invalid_data("WaterLily slot offset overflow"))?,
            )
            .ok_or_else(|| invalid_data("WaterLily slot offset overflow"))
    }

    fn required_file_len(self) -> io::Result<u64> {
        u64::from(self.header_len)
            .checked_add(
                SLOT_COUNT
                    .checked_mul(self.slot_bytes())
                    .ok_or_else(|| invalid_data("WaterLily file size overflow"))?,
            )
            .ok_or_else(|| invalid_data("WaterLily file size overflow"))
    }
}

#[derive(Debug)]
pub struct WaterlilyFrame {
    pub width: u32,
    pub height: u32,
    /// Number of depth slices; one for planar frames. Volumetric frames store
    /// `depth` tightly packed `width * height` RGBA slices ordered front
    /// (nearest the viewer) to back.
    pub depth: u32,
    pub sequence: u64,
    pub timestamp_ns: u64,
    pub rgba: Vec<u8>,
}

pub struct WaterlilyFrameReader {
    path: PathBuf,
    last_sequence: u64,
}

impl WaterlilyFrameReader {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            last_sequence: 0,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn reset(&mut self) {
        self.last_sequence = 0;
    }

    pub fn read_latest(&mut self) -> io::Result<Option<WaterlilyFrame>> {
        validate_runtime_parent(&self.path)?;
        // A predictable runtime path must never let a FIFO block the compositor
        // thread, and following a symlink would undermine the file validation
        // below. fstat after open closes the remaining type/ownership race.
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&self.path)?;
        validate_private_regular_file(&file.metadata()?)?;
        let _lock = FileLock::shared(&file)?;

        // Read the longest header both protocol versions allow; version 1
        // files can legitimately end before the volumetric extension, so a
        // short read only fails once parse() knows which version this is.
        let mut header_bytes = [0u8; WATERLILY_VOLUME_HEADER_BYTES];
        let header_read = read_prefix(&file, &mut header_bytes)?;
        let header = WaterlilyFrameHeader::parse(&header_bytes[..header_read])?;
        if header.sequence <= self.last_sequence {
            return Ok(None);
        }
        if file.metadata()?.len() < header.required_file_len()? {
            return Err(invalid_data("truncated WaterLily frame file"));
        }

        let tight_stride = usize::try_from(header.width)
            .ok()
            .and_then(|width| width.checked_mul(4))
            .ok_or_else(|| invalid_data("WaterLily tight stride overflow"))?;
        let pixel_bytes = tight_stride
            .checked_mul(header.height as usize)
            .and_then(|plane| plane.checked_mul(header.depth as usize))
            .ok_or_else(|| invalid_data("WaterLily pixel buffer overflow"))?;
        let mut rgba = vec![0u8; pixel_bytes];
        let base = header.slot_offset()?;
        if header.stride as usize == tight_stride {
            file.read_exact_at(&mut rgba, base)?;
        } else {
            // Read a padded slot in one operation, then compact it in memory.
            // Doing one pread per row is prohibitively expensive for full-screen
            // producers (for example, 1080 syscalls for each 1080p frame).
            // Depth slices are row-contiguous, so one pass over every row of
            // every slice compacts planar and volumetric slots alike.
            let slot_bytes = usize::try_from(header.slot_bytes())
                .map_err(|_| invalid_data("WaterLily slot size does not fit memory"))?;
            let mut padded = vec![0u8; slot_bytes];
            file.read_exact_at(&mut padded, base)?;
            let source_stride = header.stride as usize;
            let total_rows = header.height as usize * header.depth as usize;
            for row in 0..total_rows {
                let source = &padded[row * source_stride..row * source_stride + tight_stride];
                rgba[row * tight_stride..(row + 1) * tight_stride].copy_from_slice(source);
            }
        }

        self.last_sequence = header.sequence;
        Ok(Some(WaterlilyFrame {
            width: header.width,
            height: header.height,
            depth: header.depth,
            sequence: header.sequence,
            timestamp_ns: header.timestamp_ns,
            rgba,
        }))
    }
}

struct FileLock {
    fd: i32,
}

impl FileLock {
    fn shared(file: &File) -> io::Result<Self> {
        let fd = file.as_raw_fd();
        loop {
            // Never let a slow producer block the compositor thread. The worker
            // sends its wakeup after unlocking, so a busy file will be retried
            // by the subsequent notification.
            let result = unsafe { libc::flock(fd, libc::LOCK_SH | libc::LOCK_NB) };
            if result == 0 {
                return Ok(Self { fd });
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = unsafe { libc::flock(self.fd, libc::LOCK_UN) };
    }
}

fn validate_runtime_parent(path: &Path) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "WaterLily frame path has no parent directory",
        )
    })?;
    let metadata = std::fs::metadata(parent)?;
    let private_owner = metadata.uid() == unsafe { libc::getuid() } && metadata.mode() & 0o022 == 0;
    let sticky_shared_directory = metadata.mode() & 0o1000 != 0;
    if !metadata.is_dir() || (!private_owner && !sticky_shared_directory) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "WaterLily frame directory is neither private nor sticky",
        ));
    }
    Ok(())
}

fn validate_private_regular_file(metadata: &Metadata) -> io::Result<()> {
    if !metadata.is_file() {
        return Err(invalid_data("WaterLily frame path is not a regular file"));
    }
    if metadata.uid() != unsafe { libc::getuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "WaterLily frame file is owned by another user",
        ));
    }
    if metadata.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "WaterLily frame file must not be accessible by group or others",
        ));
    }
    Ok(())
}

/// Fill as much of `buffer` as the file holds, starting at offset zero.
/// Returns the number of bytes read; end-of-file is not an error here because
/// planar frame files may be shorter than the volumetric header.
fn read_prefix(file: &File, buffer: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match file.read_at(&mut buffer[filled..], filled as u64) {
            Ok(0) => break,
            Ok(count) => filled += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(filled)
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::OpenOptionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_FILE: AtomicU64 = AtomicU64::new(0);

    fn header(width: u32, height: u32, stride: u32, slot: u32, sequence: u64) -> [u8; 64] {
        let mut bytes = [0u8; 64];
        bytes[..8].copy_from_slice(&WATERLILY_MAGIC);
        for (offset, value) in [
            (8, WATERLILY_PROTOCOL_VERSION),
            (12, WATERLILY_HEADER_BYTES as u32),
            (16, width),
            (20, height),
            (24, stride),
            (28, WATERLILY_PIXEL_FORMAT_RGBA8),
            (32, WATERLILY_COLOR_SPACE_SRGB),
            (36, WATERLILY_ALPHA_OPAQUE),
            (40, WATERLILY_ORIGIN_TOP_LEFT),
            (44, slot),
        ] {
            bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        }
        bytes[48..56].copy_from_slice(&sequence.to_le_bytes());
        bytes[56..64].copy_from_slice(&1234u64.to_le_bytes());
        bytes
    }

    fn volume_header(
        width: u32,
        height: u32,
        depth: u32,
        stride: u32,
        slot: u32,
        sequence: u64,
    ) -> [u8; 96] {
        let mut bytes = [0u8; 96];
        bytes[..64].copy_from_slice(&header(width, height, stride, slot, sequence));
        bytes[8..12].copy_from_slice(&WATERLILY_PROTOCOL_VERSION_VOLUMETRIC.to_le_bytes());
        bytes[12..16].copy_from_slice(&(WATERLILY_VOLUME_HEADER_BYTES as u32).to_le_bytes());
        bytes[64..68].copy_from_slice(&depth.to_le_bytes());
        bytes
    }

    fn temp_frame_path() -> PathBuf {
        let id = NEXT_FILE.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("jwm-waterlily-{}-{id}.frame", std::process::id()))
    }

    #[test]
    fn parses_the_versioned_rgba_contract() {
        let parsed = WaterlilyFrameHeader::parse(&header(2, 3, 8, 1, 7)).unwrap();
        assert_eq!(
            parsed,
            WaterlilyFrameHeader {
                width: 2,
                height: 3,
                depth: 1,
                stride: 8,
                slot: 1,
                sequence: 7,
                timestamp_ns: 1234,
                header_len: WATERLILY_HEADER_BYTES as u32,
            }
        );
    }

    #[test]
    fn parses_the_volumetric_contract() {
        let parsed = WaterlilyFrameHeader::parse(&volume_header(2, 3, 5, 8, 0, 4)).unwrap();
        assert_eq!(
            parsed,
            WaterlilyFrameHeader {
                width: 2,
                height: 3,
                depth: 5,
                stride: 8,
                slot: 0,
                sequence: 4,
                timestamp_ns: 1234,
                header_len: WATERLILY_VOLUME_HEADER_BYTES as u32,
            }
        );
    }

    #[test]
    fn rejects_unsupported_or_dangerous_headers() {
        let mut bad = header(2, 3, 8, 0, 1);
        bad[28..32].copy_from_slice(&2u32.to_le_bytes());
        assert!(WaterlilyFrameHeader::parse(&bad).is_err());
        assert!(WaterlilyFrameHeader::parse(&header(0, 3, 8, 0, 1)).is_err());
        assert!(WaterlilyFrameHeader::parse(&header(2, 3, 7, 0, 1)).is_err());
        assert!(WaterlilyFrameHeader::parse(&header(2, 3, 8, 2, 1)).is_err());
        // A volumetric header must carry its extension: zero depth, a bare
        // 64-byte prefix, and a version-1 header length are all rejected.
        assert!(WaterlilyFrameHeader::parse(&volume_header(2, 3, 0, 8, 0, 1)).is_err());
        assert!(WaterlilyFrameHeader::parse(&volume_header(2, 3, 4, 8, 0, 1)[..64]).is_err());
        let mut short = volume_header(2, 3, 4, 8, 0, 1);
        short[12..16].copy_from_slice(&(WATERLILY_HEADER_BYTES as u32).to_le_bytes());
        assert!(WaterlilyFrameHeader::parse(&short).is_err());
    }

    #[test]
    fn reader_selects_the_published_slot_and_drops_old_sequences() {
        let path = temp_frame_path();
        let file = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        file.set_len(64 + 2 * 16).unwrap();
        file.write_all_at(&header(2, 2, 8, 1, 9), 0).unwrap();
        file.write_all_at(&[1u8; 16], 64).unwrap();
        file.write_all_at(&[2u8; 16], 80).unwrap();

        let mut reader = WaterlilyFrameReader::new(path.clone());
        let frame = reader.read_latest().unwrap().unwrap();
        assert_eq!(frame.sequence, 9);
        assert_eq!(frame.rgba, vec![2u8; 16]);
        assert!(reader.read_latest().unwrap().is_none());

        // A new producer connection resets the publication epoch, allowing a
        // restarted worker to begin its sequence at one.
        file.write_all_at(&header(2, 2, 8, 0, 1), 0).unwrap();
        reader.reset();
        let restarted = reader.read_latest().unwrap().unwrap();
        assert_eq!(restarted.sequence, 1);
        assert_eq!(restarted.rgba, vec![1u8; 16]);

        drop(file);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn reader_compacts_padded_rows() {
        let path = temp_frame_path();
        let file = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        file.set_len(64 + 2 * 24).unwrap();
        file.write_all_at(&header(2, 2, 12, 0, 1), 0).unwrap();
        file.write_all_at(
            &[
                1, 2, 3, 4, 5, 6, 7, 8, 99, 99, 99, 99, 9, 10, 11, 12, 13, 14, 15, 16, 88, 88, 88,
                88,
            ],
            64,
        )
        .unwrap();

        let mut reader = WaterlilyFrameReader::new(path.clone());
        let frame = reader.read_latest().unwrap().unwrap();
        assert_eq!(
            frame.rgba,
            vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]
        );

        drop(file);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn reader_returns_tight_volumetric_slabs() {
        let path = temp_frame_path();
        let file = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        // 1x2 pixels, 3 depth slices, stride padded 8 -> 12: each slot holds
        // six rows of 12 bytes behind the 96-byte volumetric header.
        file.set_len(96 + 2 * 72).unwrap();
        file.write_all_at(&volume_header(1, 2, 3, 12, 1, 6), 0)
            .unwrap();
        let mut slot = [0u8; 72];
        for row in 0..6 {
            for byte in 0..4 {
                slot[row * 12 + byte] = (row * 4 + byte + 1) as u8;
            }
        }
        file.write_all_at(&[0u8; 72], 96).unwrap();
        file.write_all_at(&slot, 96 + 72).unwrap();

        let mut reader = WaterlilyFrameReader::new(path.clone());
        let frame = reader.read_latest().unwrap().unwrap();
        assert_eq!(frame.depth, 3);
        assert_eq!(frame.sequence, 6);
        assert_eq!(frame.rgba, (1..=24).collect::<Vec<u8>>());

        drop(file);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn reader_rejects_a_fifo_without_blocking() {
        let path = temp_frame_path();
        let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);

        let mut reader = WaterlilyFrameReader::new(path.clone());
        assert!(reader.read_latest().is_err());

        std::fs::remove_file(path).unwrap();
    }
}
