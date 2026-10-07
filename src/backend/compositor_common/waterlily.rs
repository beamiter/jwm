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
/// Version 3 keeps the version-2 header layout and doubles each slot so an
/// RGBA8 material plane (octahedral normal + thickness) sits behind color.
pub const WATERLILY_PROTOCOL_VERSION_VOLUME_MATERIAL: u32 = 3;
/// Opt-in version 4 appends bounded analytic jelly poses after the two planes
/// in each slot. Producers use this only after `capabilities jelly-pose-v1`.
pub const WATERLILY_PROTOCOL_VERSION_JELLY_POSE: u32 = 4;
pub const MAX_WATERLILY_JELLIES: u32 = 5;
pub const WATERLILY_JELLY_POSE_BYTES: u32 = 32;
const WATERLILY_JELLY_POSE_KIND: u32 = 1;
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
/// Volumes also require occupancy working sets and GPU storage on the
/// compositor thread. Keep their padded publication slot bounded before the
/// reader allocates either its tight RGBA output or a stride-compaction
/// buffer. Planar frames retain the broader transport limit above.
pub const MAX_WATERLILY_VOLUME_BYTES: usize = 64 * 1024 * 1024;

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
    /// Version-3 slots carry a second RGBA plane (material) behind color.
    pub has_material: bool,
    /// Version-4 records appended to each slot; zero for legacy frames.
    pub jelly_count: u32,
    /// Bounded tessellation quality (1 or 2), zero without poses.
    pub jelly_detail: u32,
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
        let (expected_header, depth, has_material, jelly_count, jelly_detail) = match version {
            WATERLILY_PROTOCOL_VERSION => (WATERLILY_HEADER_BYTES, 1, false, 0, 0),
            WATERLILY_PROTOCOL_VERSION_VOLUMETRIC => {
                if bytes.len() < WATERLILY_VOLUME_HEADER_BYTES {
                    return Err(invalid_data("truncated WaterLily volumetric header"));
                }
                (
                    WATERLILY_VOLUME_HEADER_BYTES,
                    read_u32(bytes, 64),
                    false,
                    0,
                    0,
                )
            }
            WATERLILY_PROTOCOL_VERSION_VOLUME_MATERIAL | WATERLILY_PROTOCOL_VERSION_JELLY_POSE => {
                if bytes.len() < WATERLILY_VOLUME_HEADER_BYTES {
                    return Err(invalid_data("truncated WaterLily volumetric header"));
                }
                let depth = read_u32(bytes, 64);
                let material_flag = read_u32(bytes, 68);
                if material_flag != 1 {
                    return Err(invalid_data("WaterLily material flag must be one"));
                }
                let (jelly_count, jelly_detail) =
                    if version == WATERLILY_PROTOCOL_VERSION_JELLY_POSE {
                        let count = read_u32(bytes, 72);
                        let detail = read_u32(bytes, 84);
                        if depth <= 1
                            || !(1..=MAX_WATERLILY_JELLIES).contains(&count)
                            || read_u32(bytes, 76) != WATERLILY_JELLY_POSE_BYTES
                            || read_u32(bytes, 80) != WATERLILY_JELLY_POSE_KIND
                            || !(1..=2).contains(&detail)
                            || read_u32(bytes, 88) != 0
                            || read_u32(bytes, 92) != 0
                        {
                            return Err(invalid_data("invalid WaterLily jelly pose layout"));
                        }
                        (count, detail)
                    } else {
                        (0, 0)
                    };
                (
                    WATERLILY_VOLUME_HEADER_BYTES,
                    depth,
                    true,
                    jelly_count,
                    jelly_detail,
                )
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

        // Color plane size drives the compositor volume ceiling; the published
        // slot may be twice that when a material aux plane is present.
        let color_bytes = u64::from(stride)
            .checked_mul(u64::from(height))
            .and_then(|plane| plane.checked_mul(u64::from(depth)))
            .ok_or_else(|| invalid_data("WaterLily slot size overflow"))?;
        let slot_bytes = if has_material {
            color_bytes
                .checked_mul(2)
                .ok_or_else(|| invalid_data("WaterLily slot size overflow"))?
        } else {
            color_bytes
        };
        let slot_bytes = slot_bytes
            .checked_add(u64::from(jelly_count) * u64::from(WATERLILY_JELLY_POSE_BYTES))
            .ok_or_else(|| invalid_data("WaterLily pose slot size overflow"))?;
        if slot_bytes > MAX_FRAME_BYTES {
            return Err(invalid_data("WaterLily frame exceeds the transport limit"));
        }
        if depth > 1 && color_bytes > MAX_WATERLILY_VOLUME_BYTES as u64 {
            return Err(invalid_data(
                "WaterLily volume exceeds the compositor limit",
            ));
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
            has_material,
            jelly_count,
            jelly_detail,
        })
    }

    fn color_bytes(self) -> u64 {
        u64::from(self.stride) * u64::from(self.height) * u64::from(self.depth)
    }

    fn slot_bytes(self) -> u64 {
        let color = self.color_bytes();
        let planes = if self.has_material { color * 2 } else { color };
        planes + u64::from(self.jelly_count) * u64::from(WATERLILY_JELLY_POSE_BYTES)
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

/// Analytic jelly animation in world (right, up, depth) coordinates normalized
/// by the longest tank side. Angles are wrapped, so malformed frame data cannot
/// cause unbounded geometry or unstable trigonometry in the renderer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct JellyPose {
    pub center: [f32; 3],
    pub radius: f32,
    pub squeeze: f32,
    pub theta: f32,
    pub axis_shift: f32,
    pub mouth_y: f32,
}

impl JellyPose {
    fn parse(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() != WATERLILY_JELLY_POSE_BYTES as usize {
            return Err(invalid_data("truncated WaterLily jelly pose"));
        }
        let values: [f32; 8] = std::array::from_fn(|i| {
            f32::from_le_bytes(bytes[i * 4..i * 4 + 4].try_into().unwrap())
        });
        if values.iter().any(|value| !value.is_finite())
            || values[..3].iter().any(|value| value.abs() > 0.5)
            || !(0.0 < values[3] && values[3] <= 0.25)
            || !(0.8..=1.2).contains(&values[4])
            || values[5].abs() > std::f32::consts::TAU
            || values[6].abs() > 0.25
            || values[7].abs() > 0.5
        {
            return Err(invalid_data("WaterLily jelly pose is out of range"));
        }
        Ok(Self {
            center: [values[0], values[1], values[2]],
            radius: values[3],
            squeeze: values[4],
            theta: values[5],
            axis_shift: values[6],
            mouth_y: values[7],
        })
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
    /// Version-3 material plane: same tight dimensions as `rgba`. RG =
    /// octahedral normal in [0,1], B = thickness, A = validity (>127 valid).
    pub material: Option<Vec<u8>>,
    /// Optional version-4 analytic anatomy. Legacy frames leave this empty.
    pub jellies: Vec<JellyPose>,
    pub jelly_detail: u32,
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

        // Read the longest header all protocol versions allow; version 1
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
        let mut material = if header.has_material {
            Some(vec![0u8; pixel_bytes])
        } else {
            None
        };
        let base = header.slot_offset()?;
        if header.stride as usize == tight_stride {
            file.read_exact_at(&mut rgba, base)?;
            if let Some(ref mut material) = material {
                file.read_exact_at(material, base + pixel_bytes as u64)?;
            }
        } else {
            // Read a padded slot in one operation, then compact it in memory.
            // Doing one pread per row is prohibitively expensive for full-screen
            // producers (for example, 1080 syscalls for each 1080p frame).
            // Depth slices are row-contiguous, so one pass over every row of
            // every slice compacts planar and volumetric slots alike. Version-3
            // slots append a second padded plane immediately behind color.
            let slot_bytes = usize::try_from(header.slot_bytes())
                .map_err(|_| invalid_data("WaterLily slot size does not fit memory"))?;
            let color_plane = usize::try_from(header.color_bytes())
                .map_err(|_| invalid_data("WaterLily color plane does not fit memory"))?;
            let mut padded = vec![0u8; slot_bytes];
            file.read_exact_at(&mut padded, base)?;
            let source_stride = header.stride as usize;
            let total_rows = header.height as usize * header.depth as usize;
            for row in 0..total_rows {
                let source = &padded[row * source_stride..row * source_stride + tight_stride];
                rgba[row * tight_stride..(row + 1) * tight_stride].copy_from_slice(source);
            }
            if let Some(ref mut material) = material {
                let material_padded = &padded[color_plane..];
                for row in 0..total_rows {
                    let source =
                        &material_padded[row * source_stride..row * source_stride + tight_stride];
                    material[row * tight_stride..(row + 1) * tight_stride].copy_from_slice(source);
                }
            }
        }

        let mut jellies = Vec::with_capacity(header.jelly_count as usize);
        if header.jelly_count > 0 {
            // Keep poses and the color/material pair under the same shared
            // lock, so a frame never mixes geometry from another publication.
            let mut poses =
                [0u8; MAX_WATERLILY_JELLIES as usize * WATERLILY_JELLY_POSE_BYTES as usize];
            let pose_bytes = header.jelly_count as usize * WATERLILY_JELLY_POSE_BYTES as usize;
            file.read_exact_at(&mut poses[..pose_bytes], base + header.color_bytes() * 2)?;
            for bytes in poses[..pose_bytes]
                .as_chunks::<{ WATERLILY_JELLY_POSE_BYTES as usize }>()
                .0
            {
                jellies.push(JellyPose::parse(bytes)?);
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
            material,
            jellies,
            jelly_detail: header.jelly_detail,
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

    fn volume_material_header(
        width: u32,
        height: u32,
        depth: u32,
        stride: u32,
        slot: u32,
        sequence: u64,
    ) -> [u8; 96] {
        let mut bytes = volume_header(width, height, depth, stride, slot, sequence);
        bytes[8..12].copy_from_slice(&WATERLILY_PROTOCOL_VERSION_VOLUME_MATERIAL.to_le_bytes());
        bytes[68..72].copy_from_slice(&1u32.to_le_bytes());
        bytes
    }

    fn jelly_header(stride: u32, slot: u32, count: u32) -> [u8; 96] {
        let mut bytes = volume_material_header(1, 1, 2, stride, slot, 7);
        for (offset, value) in [
            (8, WATERLILY_PROTOCOL_VERSION_JELLY_POSE),
            (72, count),
            (76, WATERLILY_JELLY_POSE_BYTES),
            (80, WATERLILY_JELLY_POSE_KIND),
            (84, 2),
        ] {
            bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        }
        bytes
    }

    fn pose_bytes(values: [f32; 8]) -> Vec<u8> {
        values.into_iter().flat_map(f32::to_le_bytes).collect()
    }

    fn valid_pose() -> [f32; 8] {
        [0.1, -0.2, 0.3, 0.08, 1.1, -2.0, 0.01, -0.1]
    }

    #[test]
    fn jelly_layout_is_bounded_and_legacy_headers_remain_unchanged() {
        for count in 1..=MAX_WATERLILY_JELLIES {
            let parsed = WaterlilyFrameHeader::parse(&jelly_header(4, 1, count)).unwrap();
            assert_eq!(parsed.jelly_count, count);
            assert_eq!(parsed.jelly_detail, 2);
            assert_eq!(parsed.slot_bytes(), 16 + u64::from(count) * 32);
            assert_eq!(parsed.slot_offset().unwrap(), 96 + parsed.slot_bytes());
            assert_eq!(
                parsed.required_file_len().unwrap(),
                96 + 2 * parsed.slot_bytes()
            );
        }
        for (offset, value) in [
            (64, 1),
            (68, 0),
            (72, 0),
            (72, 6),
            (72, u32::MAX),
            (76, 0),
            (76, 31),
            (76, 33),
            (80, 0),
            (80, 2),
            (84, 0),
            (84, 3),
            (88, 1),
            (92, 1),
        ] {
            let mut bytes = jelly_header(4, 0, 5);
            bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
            assert!(
                WaterlilyFrameHeader::parse(&bytes).is_err(),
                "offset {offset}, value {value}"
            );
        }
        for length in [0, 63, 64, 72, 95] {
            assert!(WaterlilyFrameHeader::parse(&jelly_header(4, 0, 5)[..length]).is_err());
        }
        for legacy in [
            volume_header(1, 1, 2, 4, 0, 1),
            volume_material_header(1, 1, 2, 4, 0, 1),
        ] {
            let parsed = WaterlilyFrameHeader::parse(&legacy).unwrap();
            assert_eq!(parsed.jelly_count, 0);
            assert_eq!(parsed.jelly_detail, 0);
        }
    }

    #[test]
    fn jelly_pose_rejects_nonfinite_and_out_of_range_values() {
        assert!(JellyPose::parse(&pose_bytes(valid_pose())).is_ok());
        assert!(JellyPose::parse(&[0; 31]).is_err());
        assert!(JellyPose::parse(&[0; 33]).is_err());
        for index in 0..8 {
            for invalid in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
                let mut values = valid_pose();
                values[index] = invalid;
                assert!(JellyPose::parse(&pose_bytes(values)).is_err());
            }
        }
        for (index, invalid) in [
            (0, 0.51),
            (1, -0.51),
            (2, 0.51),
            (3, 0.0),
            (3, -0.01),
            (3, 0.251),
            (4, 0.79),
            (4, 1.21),
            (5, 6.3),
            (5, -6.3),
            (6, 0.251),
            (6, -0.251),
            (7, 0.501),
            (7, -0.501),
        ] {
            let mut values = valid_pose();
            values[index] = invalid;
            assert!(JellyPose::parse(&pose_bytes(values)).is_err());
        }
        for values in [
            [
                -0.5,
                0.5,
                -0.5,
                0.25,
                0.8,
                std::f32::consts::TAU,
                -0.25,
                0.5,
            ],
            [
                0.5,
                -0.5,
                0.5,
                f32::MIN_POSITIVE,
                1.2,
                -std::f32::consts::TAU,
                0.25,
                -0.5,
            ],
        ] {
            assert!(JellyPose::parse(&pose_bytes(values)).is_ok());
        }
    }

    #[test]
    fn reader_keeps_poses_in_the_selected_double_buffer_slot() {
        for stride in [4, 12] {
            for count in [1, MAX_WATERLILY_JELLIES] {
                let path = temp_frame_path();
                let file = OpenOptions::new()
                    .create_new(true)
                    .read(true)
                    .write(true)
                    .mode(0o600)
                    .open(&path)
                    .unwrap();
                let bytes = jelly_header(stride, 1, count);
                let header = WaterlilyFrameHeader::parse(&bytes).unwrap();
                file.set_len(header.required_file_len().unwrap()).unwrap();
                file.write_all_at(&bytes, 0).unwrap();
                let base = header.slot_offset().unwrap();
                for row in 0..2 {
                    file.write_all_at(&[1, 2, 3, 4], base + row * u64::from(stride))
                        .unwrap();
                    file.write_all_at(
                        &[128, 129, 130, 255],
                        base + header.color_bytes() + row * u64::from(stride),
                    )
                    .unwrap();
                }
                let poses = pose_bytes(valid_pose()).repeat(count as usize);
                let pose_offset = base + 2 * header.color_bytes();
                file.write_all_at(&poses, pose_offset).unwrap();
                // An incomplete inactive slot still makes the advertised file
                // invalid; validate the complete double-buffer allocation.
                file.set_len(header.required_file_len().unwrap() - 1)
                    .unwrap();
                let mut reader = WaterlilyFrameReader::new(path.clone());
                assert!(reader.read_latest().is_err());
                file.set_len(header.required_file_len().unwrap()).unwrap();
                file.write_all_at(&poses, pose_offset).unwrap();
                file.write_all_at(&f32::NAN.to_le_bytes(), pose_offset)
                    .unwrap();
                assert!(reader.read_latest().is_err());
                // Failure never consumes the publication sequence.
                file.write_all_at(&poses, pose_offset).unwrap();
                let frame = reader.read_latest().unwrap().unwrap();
                assert_eq!(frame.rgba, [1, 2, 3, 4].repeat(2));
                assert_eq!(frame.material.unwrap(), [128, 129, 130, 255].repeat(2));
                assert_eq!(
                    frame.jellies,
                    vec![JellyPose::parse(&pose_bytes(valid_pose())).unwrap(); count as usize]
                );
                assert_eq!(frame.jelly_detail, 2);
                assert!(reader.read_latest().unwrap().is_none());
                drop(file);
                std::fs::remove_file(path).unwrap();
            }
        }
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
                has_material: false,
                jelly_count: 0,
                jelly_detail: 0,
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
                has_material: false,
                jelly_count: 0,
                jelly_detail: 0,
            }
        );
    }

    #[test]
    fn parses_the_volume_material_contract() {
        let parsed =
            WaterlilyFrameHeader::parse(&volume_material_header(2, 3, 5, 8, 0, 4)).unwrap();
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
                has_material: true,
                jelly_count: 0,
                jelly_detail: 0,
            }
        );
        assert_eq!(parsed.slot_bytes(), parsed.color_bytes() * 2);
        let mut bad_flag = volume_material_header(2, 3, 5, 8, 0, 4);
        bad_flag[68..72].copy_from_slice(&0u32.to_le_bytes());
        assert!(WaterlilyFrameHeader::parse(&bad_flag).is_err());
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

        // Reject an oversized padded volume while it is still only a header:
        // its tight pixels are tiny, but accepting the advertised stride
        // would otherwise allocate a >64 MiB compaction buffer before the X11
        // upload layer gets a chance to apply its own defense-in-depth check.
        let padded_stride = (MAX_WATERLILY_VOLUME_BYTES / 4 + 1) as u32;
        assert!(WaterlilyFrameHeader::parse(&volume_header(1, 2, 2, padded_stride, 0, 1)).is_err());

        // The volume ceiling is deliberately tighter than the generic planar
        // transport ceiling because only volumes allocate occupancy data.
        assert!(WaterlilyFrameHeader::parse(&header(1, 2, padded_stride, 0, 1)).is_ok());
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
        assert!(frame.material.is_none());

        drop(file);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn reader_returns_volume_material_planes() {
        let path = temp_frame_path();
        let file = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        // 1x2 pixels, 3 depth slices, padded stride 12: color plane 72 bytes,
        // material plane another 72, doubled slots behind the 96-byte header.
        let color_plane = 72usize;
        let slot_bytes = color_plane * 2;
        file.set_len(96 + 2 * slot_bytes as u64).unwrap();
        file.write_all_at(&volume_material_header(1, 2, 3, 12, 1, 6), 0)
            .unwrap();
        let mut slot = vec![0u8; slot_bytes];
        for row in 0..6 {
            for byte in 0..4 {
                slot[row * 12 + byte] = (row * 4 + byte + 1) as u8;
                slot[color_plane + row * 12 + byte] = (100 + row * 4 + byte) as u8;
            }
        }
        file.write_all_at(&vec![0u8; slot_bytes], 96).unwrap();
        file.write_all_at(&slot, 96 + slot_bytes as u64).unwrap();

        let mut reader = WaterlilyFrameReader::new(path.clone());
        let frame = reader.read_latest().unwrap().unwrap();
        assert_eq!(frame.depth, 3);
        assert_eq!(frame.sequence, 6);
        assert_eq!(frame.rgba, (1..=24).collect::<Vec<u8>>());
        assert_eq!(
            frame.material.as_ref().unwrap(),
            &(100..=123).collect::<Vec<u8>>()
        );

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
