//! Dirty-tile delta coding for remote desktop frames.
//!
//! A full-frame JPEG re-encodes and re-ships the whole desktop even when a
//! single caret blinked.  Measured on a 3440x1440 JWM session, a barely-active
//! desktop cost 519 KiB per frame at native resolution while only ~1 % of
//! 16-pixel tiles actually changed.
//!
//! This module ships only the changed tiles.  Each tile that differs from the
//! host's model of the viewer's canvas is copied into one packed *atlas*
//! image, and that single atlas is JPEG-encoded.  One atlas beats one JPEG per
//! dirty rectangle by roughly 3x on real desktops, because a few hundred small
//! rectangles otherwise pay a few hundred JPEG headers and lose all shared
//! Huffman statistics.
//!
//! The reference image is deliberately *the pixels the viewer was last sent*,
//! not the previously captured frame.  Comparing against the last transmitted
//! content means a small per-channel tolerance cannot accumulate: a region
//! drifting slowly still crosses the tolerance against its own stale copy and
//! is retransmitted.  It also makes committing a frame proportional to the
//! dirty area instead of the whole image.
//!
//! Payload layout (all integers big-endian):
//!
//! ```text
//! sequence       u64
//! source_width   u16   source_height  u16
//! encoded_width  u16   encoded_height u16
//! tile_log2      u8    flags          u8
//! atlas_columns  u16
//! dirty_tiles    u32
//! bitmap         ceil(tiles_across * tiles_down / 8) bytes, raster order
//! atlas          JPEG, present only when dirty_tiles > 0
//! ```

use super::RemoteResult;
use super::frame::{
    BoundedPayloadWriter, MAX_DIMENSION, RecyclableDecodedFrame, SharedDecodeBufferPool,
    decode_rgb8_into_pool, invalid_data, validate_dimensions,
};
use super::protocol::MAX_PAYLOAD_LEN;
use super::x11_capture::CapturedFrame;
use image::codecs::jpeg::{JpegDecoder, JpegEncoder};
use image::{ColorType, DynamicImage, ExtendedColorType, ImageDecoder, Limits, RgbImage};
use std::io::Cursor;

/// Byte length of the fixed tile-frame header preceding the dirty bitmap.
pub(crate) const TILE_HEADER_LEN: usize = 24;

/// Tile edge is `1 << TILE_LOG2`.  Sixteen pixels keeps every tile aligned to a
/// 4:2:0 JPEG minimum coded unit, so atlas neighbours cannot bleed chroma into
/// each other, and it measured best against 8/32/64 on real desktop content.
pub(crate) const DEFAULT_TILE_LOG2: u8 = 4;

/// Accepted tile sizes: 8..=128 pixels.
const MIN_TILE_LOG2: u8 = 3;
const MAX_TILE_LOG2: u8 = 7;

/// Per-channel tolerance below which a tile counts as unchanged.
///
/// Scaled captures dither by a unit or two between otherwise identical frames,
/// which would otherwise mark most of the screen dirty forever.  Because the
/// comparison is against the last *transmitted* pixels, the error this admits
/// is bounded by the tolerance itself rather than accumulating per frame.
pub(crate) const DEFAULT_TILE_TOLERANCE: u8 = 4;

/// Set when every tile is present, so a viewer may (re)initialise its canvas.
const FLAG_KEYFRAME: u8 = 1 << 0;
const KNOWN_FLAGS: u8 = FLAG_KEYFRAME;

/// Upper bound on atlas columns for a given tile size, keeping the packed
/// atlas inside [`MAX_DIMENSION`] even at the largest accepted tile.
fn max_atlas_columns(tile: usize) -> usize {
    (usize::from(MAX_DIMENSION) / tile).max(1)
}

/// What the sender is asking the encoder to produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TileEncodeRequest {
    /// Ordinary frame: send nothing when nothing changed.
    Delta,
    /// Send a frame even with nothing dirty, to keep the session's video
    /// liveness timer fed. An empty tile frame is a few dozen bytes.
    Keepalive,
    /// Send every tile: first frame, geometry change, or recovery.
    Keyframe,
}

/// What one planned frame owes the viewer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TilePlan {
    pub(crate) keyframe: bool,
    pub(crate) dirty_tiles: u32,
    pub(crate) total_tiles: u32,
    /// False when nothing changed and no keepalive was requested.
    pub(crate) emit: bool,
}

/// Geometry the planner resolved, consumed by the matching encode.
#[derive(Debug, Clone, Copy)]
struct PlannedFrame {
    keyframe: bool,
    dirty_tiles: u32,
    encoded: (u16, u16),
    source: (u16, u16),
    columns: usize,
    /// `None` for an empty keepalive frame, which carries no atlas.
    atlas: Option<(usize, u16, u16)>,
}

/// Host-side delta encoder holding the host's model of the viewer's canvas.
#[derive(Debug)]
pub(crate) struct TileEncoder {
    tile_log2: u8,
    tolerance: u8,
    /// Pixels the viewer has been sent.  `None` until the first commit.
    reference: Option<RgbImage>,
    source: (u16, u16),
    grid: (usize, usize),
    bitmap: Vec<u8>,
    dirty: Vec<u32>,
    atlas: Vec<u8>,
    /// Dirty set of the last successful encode, awaiting commit or discard.
    pending: bool,
    planned: Option<PlannedFrame>,
}

impl TileEncoder {
    pub(crate) fn new() -> Self {
        Self::with_settings(DEFAULT_TILE_LOG2, DEFAULT_TILE_TOLERANCE)
    }

    pub(crate) fn with_settings(tile_log2: u8, tolerance: u8) -> Self {
        Self {
            tile_log2: tile_log2.clamp(MIN_TILE_LOG2, MAX_TILE_LOG2),
            tolerance,
            reference: None,
            source: (0, 0),
            grid: (0, 0),
            bitmap: Vec::new(),
            dirty: Vec::new(),
            atlas: Vec::new(),
            pending: false,
            planned: None,
        }
    }

    fn tile(&self) -> usize {
        1_usize << self.tile_log2
    }

    /// True when the next frame must be a keyframe.
    fn needs_keyframe(&self, frame: &CapturedFrame) -> bool {
        let Some(reference) = self.reference.as_ref() else {
            return true;
        };
        reference.dimensions() != frame.image.dimensions()
            || self.source != (frame.source_width, frame.source_height)
    }

    /// Decide what the next frame owes the viewer, without encoding anything.
    ///
    /// Dirty detection is cheap and the sender must know whether a frame is
    /// worth sending *before* it spends a quality decision or a wire sequence
    /// on it, so planning is deliberately separate from encoding.
    pub(crate) fn plan(
        &mut self,
        frame: &CapturedFrame,
        request: TileEncodeRequest,
    ) -> RemoteResult<TilePlan> {
        self.planned = None;
        let encoded_width = u16::try_from(frame.image.width())
            .map_err(|_| invalid_data("encoded frame width exceeds protocol range"))?;
        let encoded_height = u16::try_from(frame.image.height())
            .map_err(|_| invalid_data("encoded frame height exceeds protocol range"))?;
        validate_dimensions(frame.source_width, frame.source_height)?;
        validate_dimensions(encoded_width, encoded_height)?;

        let tile = self.tile();
        let grid_w = usize::from(encoded_width).div_ceil(tile);
        let grid_h = usize::from(encoded_height).div_ceil(tile);
        let total_tiles = grid_w
            .checked_mul(grid_h)
            .and_then(|tiles| u32::try_from(tiles).ok())
            .ok_or_else(|| invalid_data("tile grid exceeds protocol range"))?;
        self.grid = (grid_w, grid_h);
        self.bitmap.clear();
        self.bitmap.resize(bitmap_len(total_tiles), 0);
        self.dirty.clear();

        let keyframe = request == TileEncodeRequest::Keyframe || self.needs_keyframe(frame);
        if keyframe {
            self.bitmap.iter_mut().for_each(|byte| *byte = 0xff);
            trim_bitmap_tail(&mut self.bitmap, total_tiles);
            self.dirty.extend(0..total_tiles);
        } else {
            let reference = self
                .reference
                .as_ref()
                .expect("a non-keyframe always has a reference image");
            collect_dirty(
                &frame.image,
                reference,
                (grid_w, grid_h),
                tile,
                self.tolerance,
                &mut self.bitmap,
                &mut self.dirty,
            );
        }

        let dirty_tiles = u32::try_from(self.dirty.len())
            .map_err(|_| invalid_data("dirty tile count exceeds protocol range"))?;
        let emit = dirty_tiles > 0 || request == TileEncodeRequest::Keepalive;
        if !emit {
            return Ok(TilePlan {
                keyframe,
                dirty_tiles,
                total_tiles,
                emit: false,
            });
        }

        let (columns, atlas) = if dirty_tiles == 0 {
            (0, None)
        } else {
            let columns = if keyframe {
                grid_w
            } else {
                atlas_columns(self.dirty.len(), tile)
            };
            let rows = self.dirty.len().div_ceil(columns);
            let atlas_width = u16::try_from(columns * tile)
                .ok()
                .filter(|width| *width <= MAX_DIMENSION)
                .ok_or_else(|| invalid_data("tile atlas width exceeds the safety limit"))?;
            let atlas_height = u16::try_from(rows * tile)
                .ok()
                .filter(|height| *height <= MAX_DIMENSION)
                .ok_or_else(|| invalid_data("tile atlas height exceeds the safety limit"))?;
            validate_dimensions(atlas_width, atlas_height)?;
            (columns, Some((rows, atlas_width, atlas_height)))
        };

        self.planned = Some(PlannedFrame {
            keyframe,
            dirty_tiles,
            encoded: (encoded_width, encoded_height),
            source: (frame.source_width, frame.source_height),
            columns,
            atlas,
        });
        Ok(TilePlan {
            keyframe,
            dirty_tiles,
            total_tiles,
            emit: true,
        })
    }

    /// Encode the frame most recently accepted by [`Self::plan`].
    ///
    /// The encoder state is not advanced: the caller commits only once the
    /// authenticated record and its flush have succeeded, so a torn write can
    /// never leave the host believing the viewer holds tiles it never drew.
    pub(crate) fn encode_into(
        &mut self,
        payload: &mut Vec<u8>,
        sequence: u64,
        frame: &CapturedFrame,
        quality: u8,
    ) -> RemoteResult<()> {
        payload.clear();
        let result = self.encode_inner(payload, sequence, frame, quality);
        if result.is_err() {
            payload.clear();
            self.pending = false;
            self.planned = None;
        }
        result
    }

    fn encode_inner(
        &mut self,
        payload: &mut Vec<u8>,
        sequence: u64,
        frame: &CapturedFrame,
        quality: u8,
    ) -> RemoteResult<()> {
        let planned = self
            .planned
            .ok_or_else(|| invalid_data("tile frame was encoded without a plan"))?;
        let encoded = (
            u16::try_from(frame.image.width()).unwrap_or(0),
            u16::try_from(frame.image.height()).unwrap_or(0),
        );
        if planned.encoded != encoded || planned.source != (frame.source_width, frame.source_height)
        {
            return Err(invalid_data("tile frame does not match its plan").into());
        }

        payload.extend_from_slice(&sequence.to_be_bytes());
        payload.extend_from_slice(&frame.source_width.to_be_bytes());
        payload.extend_from_slice(&frame.source_height.to_be_bytes());
        payload.extend_from_slice(&planned.encoded.0.to_be_bytes());
        payload.extend_from_slice(&planned.encoded.1.to_be_bytes());
        payload.push(self.tile_log2);
        payload.push(if planned.keyframe { FLAG_KEYFRAME } else { 0 });
        payload.extend_from_slice(
            &u16::try_from(planned.columns)
                .map_err(|_| invalid_data("tile atlas column count exceeds protocol range"))?
                .to_be_bytes(),
        );
        payload.extend_from_slice(&planned.dirty_tiles.to_be_bytes());
        payload.extend_from_slice(&self.bitmap);
        debug_assert_eq!(payload.len(), TILE_HEADER_LEN + self.bitmap.len());

        if let Some((rows, atlas_width, atlas_height)) = planned.atlas {
            self.pack_atlas(&frame.image, planned.columns, rows);
            JpegEncoder::new_with_quality(
                BoundedPayloadWriter::new(payload, MAX_PAYLOAD_LEN),
                quality.clamp(1, 100),
            )
            .encode(
                &self.atlas,
                u32::from(atlas_width),
                u32::from(atlas_height),
                ExtendedColorType::Rgb8,
            )?;
        }

        self.pending = true;
        self.planned = None;
        Ok(())
    }

    /// Adopt the encoded frame as the new model of the viewer's canvas.
    ///
    /// Only the tiles actually transmitted are copied, so a steady session
    /// touches a few percent of the reference instead of cloning it.
    pub(crate) fn commit(&mut self, frame: &CapturedFrame) {
        if !self.pending {
            return;
        }
        self.pending = false;
        self.source = (frame.source_width, frame.source_height);
        let tile = self.tile();
        let (grid_w, _) = self.grid;
        let reference = match self.reference.as_mut() {
            Some(reference) if reference.dimensions() == frame.image.dimensions() => reference,
            _ => {
                self.reference = Some(frame.image.clone());
                return;
            }
        };
        let width = frame.image.width() as usize;
        let height = frame.image.height() as usize;
        let stride = width * 3;
        let source = frame.image.as_raw();
        let destination = reference.as_mut();
        for index in &self.dirty {
            let (x0, y0, x1, y1) = tile_bounds(*index as usize, grid_w, tile, width, height);
            let bytes = (x1 - x0) * 3;
            for y in y0..y1 {
                let offset = y * stride + x0 * 3;
                destination[offset..offset + bytes]
                    .copy_from_slice(&source[offset..offset + bytes]);
            }
        }
    }

    /// Drop an encoded-but-unsent frame without advancing the reference.
    pub(crate) fn discard(&mut self) {
        self.pending = false;
        self.planned = None;
    }

    #[cfg(test)]
    pub(crate) fn reference(&self) -> Option<&RgbImage> {
        self.reference.as_ref()
    }
}

fn collect_dirty(
    current: &RgbImage,
    reference: &RgbImage,
    (grid_w, grid_h): (usize, usize),
    tile: usize,
    tolerance: u8,
    bitmap: &mut [u8],
    dirty: &mut Vec<u32>,
) {
    {
        let width = current.width() as usize;
        let height = current.height() as usize;
        let stride = width * 3;
        let current = current.as_raw();
        let reference = reference.as_raw();
        for ty in 0..grid_h {
            for tx in 0..grid_w {
                let index = ty * grid_w + tx;
                let (x0, y0, x1, y1) = tile_bounds(index, grid_w, tile, width, height);
                let bytes = (x1 - x0) * 3;
                let mut changed = false;
                for y in y0..y1 {
                    let offset = y * stride + x0 * 3;
                    let a = &current[offset..offset + bytes];
                    let b = &reference[offset..offset + bytes];
                    // Slice equality is a memcmp, which vectorises far better
                    // than a per-byte tolerance scan.  Identical rows are the
                    // overwhelmingly common case, so only differing rows pay
                    // for the tolerance comparison.
                    if a == b {
                        continue;
                    }
                    if tolerance == 0
                        || a.iter()
                            .zip(b)
                            .any(|(left, right)| left.abs_diff(*right) > tolerance)
                    {
                        changed = true;
                        break;
                    }
                }
                if changed {
                    bitmap[index / 8] |= 1 << (index % 8);
                    dirty.push(index as u32);
                }
            }
        }
    }
}

impl TileEncoder {
    fn pack_atlas(&mut self, current: &RgbImage, columns: usize, rows: usize) {
        let tile = self.tile();
        let (grid_w, _) = self.grid;
        let width = current.width() as usize;
        let height = current.height() as usize;
        let stride = width * 3;
        let atlas_stride = columns * tile * 3;
        self.atlas.clear();
        self.atlas.resize(atlas_stride * rows * tile, 0);
        let source = current.as_raw();
        for (slot, index) in self.dirty.iter().enumerate() {
            let (x0, y0, x1, y1) = tile_bounds(*index as usize, grid_w, tile, width, height);
            let cell_row = slot / columns;
            let cell_column = slot % columns;
            let cell_x = cell_column * tile * 3;
            let cell_y = cell_row * tile;
            let bytes = (x1 - x0) * 3;
            for y in 0..tile {
                let destination = (cell_y + y) * atlas_stride + cell_x;
                // Edge tiles are partial.  Replicate their last valid row and
                // column across the padding so the JPEG encoder sees a flat
                // continuation instead of a hard edge into black, which would
                // cost bytes and ring back into the visible pixels.
                let source_y = y0 + y.min(y1 - y0 - 1);
                let offset = source_y * stride + x0 * 3;
                self.atlas[destination..destination + bytes]
                    .copy_from_slice(&source[offset..offset + bytes]);
                let (filled, padding) =
                    self.atlas[destination..destination + tile * 3].split_at_mut(bytes);
                if let Some(last) = filled.last_chunk::<3>() {
                    let last = *last;
                    for pixel in padding.chunks_exact_mut(3) {
                        pixel.copy_from_slice(&last);
                    }
                }
            }
        }
    }
}

/// Viewer-side canvas that reassembles dirty tiles into whole frames.
#[derive(Debug, Default)]
pub(crate) struct TileDecoder {
    canvas: Option<RgbImage>,
    source: (u16, u16),
    atlas: Vec<u8>,
}

impl TileDecoder {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Decode one authenticated tile-frame payload into a complete frame.
    pub(crate) fn decode_into(
        &mut self,
        payload: &[u8],
        pool: SharedDecodeBufferPool,
    ) -> RemoteResult<RecyclableDecodedFrame> {
        let header = TileHeader::parse(payload)?;
        let bitmap_len = bitmap_len(header.total_tiles);
        let body_start = TILE_HEADER_LEN
            .checked_add(bitmap_len)
            .ok_or_else(|| invalid_data("tile frame header length overflow"))?;
        if payload.len() < body_start {
            return Err(invalid_data("tile frame is missing its dirty bitmap").into());
        }
        let bitmap = &payload[TILE_HEADER_LEN..body_start];
        let counted = bitmap.iter().map(|byte| byte.count_ones()).sum::<u32>();
        if counted != header.dirty_tiles {
            return Err(
                invalid_data("tile frame bitmap does not match its declared dirty count").into(),
            );
        }
        if header.keyframe && header.dirty_tiles != header.total_tiles {
            return Err(invalid_data("tile keyframe does not carry every tile").into());
        }

        let canvas_dimensions = (
            u32::from(header.encoded_width),
            u32::from(header.encoded_height),
        );
        if header.keyframe {
            // A keyframe is self-contained; adopt its geometry unconditionally.
            match self.canvas.as_mut() {
                Some(canvas) if canvas.dimensions() == canvas_dimensions => {}
                _ => {
                    self.canvas = Some(RgbImage::new(canvas_dimensions.0, canvas_dimensions.1));
                }
            }
        }
        self.source = (header.source_width, header.source_height);
        let Self { canvas, atlas, .. } = self;
        let canvas = canvas
            .as_mut()
            .filter(|canvas| canvas.dimensions() == canvas_dimensions)
            .ok_or_else(|| {
                invalid_data("tile delta frame does not match the current viewer canvas")
            })?;

        if header.dirty_tiles > 0 {
            let tile = 1_usize << header.tile_log2;
            let columns = usize::from(header.atlas_columns);
            if columns == 0 {
                return Err(invalid_data("tile frame declares no atlas columns").into());
            }
            let dirty = header.dirty_tiles as usize;
            if columns > dirty {
                return Err(
                    invalid_data("tile frame atlas is wider than its dirty tile count").into(),
                );
            }
            let rows = dirty.div_ceil(columns);
            let atlas_width = u16::try_from(columns * tile)
                .map_err(|_| invalid_data("tile atlas width exceeds protocol range"))?;
            let atlas_height = u16::try_from(rows * tile)
                .map_err(|_| invalid_data("tile atlas height exceeds protocol range"))?;
            validate_dimensions(atlas_width, atlas_height)?;
            if payload.len() <= body_start {
                return Err(invalid_data("tile frame is missing its atlas body").into());
            }
            decode_atlas(
                atlas,
                &payload[body_start..],
                atlas_width,
                atlas_height,
                payload.len(),
            )?;
            scatter_atlas(
                canvas,
                atlas,
                bitmap,
                header.total_tiles,
                tile,
                columns,
                atlas_width,
            );
        }

        let expected_len = (canvas.as_raw().len(), canvas.dimensions());
        let image = clone_into_pool(canvas, &pool)?;
        debug_assert_eq!((image.as_raw().len(), image.dimensions()), expected_len);
        Ok(RecyclableDecodedFrame::new(
            header.sequence,
            header.source_width,
            header.source_height,
            image,
            pool,
        ))
    }

    #[cfg(test)]
    pub(crate) fn canvas(&self) -> Option<&RgbImage> {
        self.canvas.as_ref()
    }
}

fn decode_atlas(
    atlas: &mut Vec<u8>,
    body: &[u8],
    atlas_width: u16,
    atlas_height: u16,
    payload_len: usize,
) -> RemoteResult<()> {
    {
        let mut decoder = JpegDecoder::new(Cursor::new(body))?;
        let expected = (u32::from(atlas_width), u32::from(atlas_height));
        if decoder.dimensions() != expected {
            return Err(invalid_data(
                "tile atlas JPEG dimensions do not match the authenticated header",
            )
            .into());
        }
        let expected_len = usize::from(atlas_width)
            .checked_mul(usize::from(atlas_height))
            .and_then(|pixels| pixels.checked_mul(3))
            .ok_or_else(|| invalid_data("tile atlas length overflow"))?;
        let mut limits = Limits::default();
        limits.max_image_width = Some(expected.0);
        limits.max_image_height = Some(expected.1);
        limits.max_alloc = Some((expected_len as u64).saturating_add(payload_len as u64));
        decoder.set_limits(limits)?;

        if decoder.color_type() == ColorType::Rgb8 {
            if decoder.total_bytes() != expected_len as u64 {
                return Err(
                    invalid_data("tile atlas byte length does not match its dimensions").into(),
                );
            }
            atlas.clear();
            atlas.resize(expected_len, 0);
            if let Err(error) = decoder.read_image(atlas) {
                // A partial decode leaves stale pixels behind; make sure they
                // can never be scattered into the canvas.
                atlas.clear();
                return Err(error.into());
            }
        } else {
            let image = DynamicImage::from_decoder(decoder)?.into_rgb8();
            if image.dimensions() != expected {
                return Err(invalid_data(
                    "tile atlas JPEG dimensions do not match the authenticated header",
                )
                .into());
            }
            atlas.clear();
            atlas.extend_from_slice(image.as_raw());
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct TileHeader {
    sequence: u64,
    source_width: u16,
    source_height: u16,
    encoded_width: u16,
    encoded_height: u16,
    tile_log2: u8,
    keyframe: bool,
    atlas_columns: u16,
    dirty_tiles: u32,
    total_tiles: u32,
}

impl TileHeader {
    fn parse(payload: &[u8]) -> RemoteResult<Self> {
        if payload.len() < TILE_HEADER_LEN {
            return Err(invalid_data("tile frame is shorter than its header").into());
        }
        let sequence = u64::from_be_bytes(payload[0..8].try_into().unwrap());
        let source_width = u16::from_be_bytes(payload[8..10].try_into().unwrap());
        let source_height = u16::from_be_bytes(payload[10..12].try_into().unwrap());
        let encoded_width = u16::from_be_bytes(payload[12..14].try_into().unwrap());
        let encoded_height = u16::from_be_bytes(payload[14..16].try_into().unwrap());
        let tile_log2 = payload[16];
        let flags = payload[17];
        let atlas_columns = u16::from_be_bytes(payload[18..20].try_into().unwrap());
        let dirty_tiles = u32::from_be_bytes(payload[20..24].try_into().unwrap());

        validate_dimensions(source_width, source_height)?;
        validate_dimensions(encoded_width, encoded_height)?;
        if !(MIN_TILE_LOG2..=MAX_TILE_LOG2).contains(&tile_log2) {
            return Err(invalid_data("tile frame declares an unsupported tile size").into());
        }
        if flags & !KNOWN_FLAGS != 0 {
            return Err(invalid_data("tile frame sets unknown flags").into());
        }
        let tile = 1_usize << tile_log2;
        let total_tiles = usize::from(encoded_width)
            .div_ceil(tile)
            .checked_mul(usize::from(encoded_height).div_ceil(tile))
            .and_then(|tiles| u32::try_from(tiles).ok())
            .ok_or_else(|| invalid_data("tile grid exceeds protocol range"))?;
        if dirty_tiles > total_tiles {
            return Err(invalid_data("tile frame declares more dirty tiles than exist").into());
        }
        Ok(Self {
            sequence,
            source_width,
            source_height,
            encoded_width,
            encoded_height,
            tile_log2,
            keyframe: flags & FLAG_KEYFRAME != 0,
            atlas_columns,
            dirty_tiles,
            total_tiles,
        })
    }
}

fn scatter_atlas(
    canvas: &mut RgbImage,
    atlas: &[u8],
    bitmap: &[u8],
    total_tiles: u32,
    tile: usize,
    columns: usize,
    atlas_width: u16,
) {
    let width = canvas.width() as usize;
    let height = canvas.height() as usize;
    let grid_w = width.div_ceil(tile);
    let stride = width * 3;
    let atlas_stride = usize::from(atlas_width) * 3;
    let destination = canvas.as_mut();
    let mut slot = 0_usize;
    for index in 0..total_tiles as usize {
        if bitmap[index / 8] & (1 << (index % 8)) == 0 {
            continue;
        }
        let (x0, y0, x1, y1) = tile_bounds(index, grid_w, tile, width, height);
        let bytes = (x1 - x0) * 3;
        let cell_x = (slot % columns) * tile * 3;
        let cell_y = (slot / columns) * tile;
        for y in y0..y1 {
            let from = (cell_y + (y - y0)) * atlas_stride + cell_x;
            let to = y * stride + x0 * 3;
            destination[to..to + bytes].copy_from_slice(&atlas[from..from + bytes]);
        }
        slot += 1;
    }
}

fn clone_into_pool(canvas: &RgbImage, pool: &SharedDecodeBufferPool) -> RemoteResult<RgbImage> {
    let raw = canvas.as_raw();
    decode_rgb8_into_pool(canvas.width(), canvas.height(), raw.len(), pool, |pixels| {
        pixels.copy_from_slice(raw);
        Ok(())
    })
}

fn tile_bounds(
    index: usize,
    grid_w: usize,
    tile: usize,
    width: usize,
    height: usize,
) -> (usize, usize, usize, usize) {
    let ty = index / grid_w;
    let tx = index % grid_w;
    let x0 = tx * tile;
    let y0 = ty * tile;
    ((x0), (y0), (x0 + tile).min(width), (y0 + tile).min(height))
}

fn bitmap_len(total_tiles: u32) -> usize {
    (total_tiles as usize).div_ceil(8)
}

fn trim_bitmap_tail(bitmap: &mut [u8], total_tiles: u32) {
    let remainder = (total_tiles % 8) as usize;
    if remainder != 0
        && let Some(last) = bitmap.last_mut()
    {
        *last = (1_u8 << remainder) - 1;
    }
}

fn atlas_columns(dirty: usize, tile: usize) -> usize {
    // A near-square atlas keeps both dimensions small; JPEG cares far more
    // about total area than aspect, and a single row of hundreds of tiles
    // would blow past the protocol width limit.
    //
    // Widening also has to bound the *row* count: clamping columns alone would
    // trade an over-wide atlas for an over-tall one. Any image inside
    // `MAX_PIXELS` fits comfortably; the encoder still validates and fails
    // closed rather than emitting an atlas it cannot describe.
    let limit = max_atlas_columns(tile);
    let widest_for_rows = dirty.div_ceil(limit).max(1);
    dirty
        .isqrt()
        .max(1)
        .max(widest_for_rows)
        .min(limit)
        .min(dirty.max(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::frame::{MAX_PIXELS, new_decode_buffer_pool};
    use image::Rgb;

    fn frame(width: u32, height: u32, fill: impl Fn(u32, u32) -> Rgb<u8>) -> CapturedFrame {
        let mut image = RgbImage::new(width, height);
        for (x, y, pixel) in image.enumerate_pixels_mut() {
            *pixel = fill(x, y);
        }
        CapturedFrame {
            image,
            source_width: u16::try_from(width * 2).unwrap(),
            source_height: u16::try_from(height * 2).unwrap(),
        }
    }

    fn flat(width: u32, height: u32, value: u8) -> CapturedFrame {
        frame(width, height, |_, _| Rgb([value, value, value]))
    }

    /// Deterministic high-frequency content. A flat fill compresses so well
    /// that it cannot show what delta coding saves on a real desktop.
    fn busy_shade(x: u32, y: u32) -> u8 {
        ((x.wrapping_mul(37) ^ y.wrapping_mul(101)).wrapping_add(x * y) % 251) as u8
    }

    fn busy(width: u32, height: u32) -> CapturedFrame {
        frame(width, height, |x, y| {
            let base = busy_shade(x, y);
            Rgb([base, base.wrapping_add(53), base.wrapping_add(151)])
        })
    }

    /// Round-trip one frame through encoder and decoder, committing on success.
    fn roundtrip(
        encoder: &mut TileEncoder,
        decoder: &mut TileDecoder,
        source: &CapturedFrame,
        quality: u8,
        request: TileEncodeRequest,
    ) -> (TilePlan, usize, Option<RgbImage>) {
        let mut payload = Vec::new();
        let plan = encoder.plan(source, request).expect("plan succeeds");
        if !plan.emit {
            return (plan, 0, None);
        }
        encoder
            .encode_into(&mut payload, 7, source, quality)
            .expect("encode succeeds");
        let decoded = decoder
            .decode_into(&payload, new_decode_buffer_pool())
            .expect("decode succeeds");
        let image = decoded.image().clone();
        encoder.commit(source);
        (plan, payload.len(), Some(image))
    }

    /// Plan-then-encode in one step, for tests that only need the bytes.
    fn encode(
        encoder: &mut TileEncoder,
        payload: &mut Vec<u8>,
        sequence: u64,
        frame: &CapturedFrame,
        quality: u8,
        request: TileEncodeRequest,
    ) -> TilePlan {
        let plan = encoder.plan(frame, request).expect("plan succeeds");
        if plan.emit {
            encoder
                .encode_into(payload, sequence, frame, quality)
                .expect("encode succeeds");
        } else {
            payload.clear();
        }
        plan
    }

    fn max_channel_error(left: &RgbImage, right: &RgbImage) -> u8 {
        assert_eq!(left.dimensions(), right.dimensions());
        left.as_raw()
            .iter()
            .zip(right.as_raw())
            .map(|(a, b)| a.abs_diff(*b))
            .max()
            .unwrap_or(0)
    }

    #[test]
    fn a_keyframe_reconstructs_the_whole_image() {
        let mut encoder = TileEncoder::new();
        let mut decoder = TileDecoder::new();
        let source = flat(160, 96, 200);
        let (plan, _, image) = roundtrip(
            &mut encoder,
            &mut decoder,
            &source,
            95,
            TileEncodeRequest::Delta,
        );
        assert_eq!(
            plan,
            TilePlan {
                keyframe: true,
                dirty_tiles: 60,
                total_tiles: 60,
                emit: true,
            }
        );
        let image = image.expect("a keyframe always decodes");
        assert_eq!(image.dimensions(), source.image.dimensions());
        assert!(max_channel_error(&image, &source.image) <= 4);
    }

    #[test]
    fn an_unchanged_frame_produces_no_payload() {
        let mut encoder = TileEncoder::new();
        let mut decoder = TileDecoder::new();
        let source = flat(160, 96, 128);
        roundtrip(
            &mut encoder,
            &mut decoder,
            &source,
            90,
            TileEncodeRequest::Delta,
        );

        let mut payload = vec![0xAB; 32];
        let plan = encode(
            &mut encoder,
            &mut payload,
            8,
            &source,
            90,
            TileEncodeRequest::Delta,
        );
        assert!(!plan.emit, "an unchanged delta sends nothing");
        assert_eq!(plan.dirty_tiles, 0);
        assert!(payload.is_empty(), "an unchanged frame clears its payload");

        // The same unchanged capture must still produce a tiny liveness frame
        // when the session asks for one.
        let plan = encode(
            &mut encoder,
            &mut payload,
            8,
            &source,
            90,
            TileEncodeRequest::Keepalive,
        );
        assert!(plan.emit && plan.dirty_tiles == 0);
        assert_eq!(
            payload.len(),
            TILE_HEADER_LEN + bitmap_len(plan.total_tiles),
            "an empty keepalive carries a header and bitmap but no atlas"
        );
        let decoded = decoder
            .decode_into(&payload, new_decode_buffer_pool())
            .expect("an empty keepalive decodes to the unchanged canvas");
        assert_eq!(decoded.image().dimensions(), source.image.dimensions());
    }

    #[test]
    fn only_the_changed_tiles_are_transmitted() {
        let mut encoder = TileEncoder::new();
        let mut decoder = TileDecoder::new();
        let first = busy(320, 192);
        let (_, keyframe_len, _) = roundtrip(
            &mut encoder,
            &mut decoder,
            &first,
            80,
            TileEncodeRequest::Delta,
        );

        // Repaint exactly one 16x16 tile.
        let mut second = busy(320, 192);
        for y in 32..48 {
            for x in 64..80 {
                second.image.put_pixel(x, y, Rgb([250, 10, 10]));
            }
        }
        let (plan, delta_len, image) = roundtrip(
            &mut encoder,
            &mut decoder,
            &second,
            80,
            TileEncodeRequest::Delta,
        );
        assert_eq!(
            plan,
            TilePlan {
                keyframe: false,
                dirty_tiles: 1,
                total_tiles: 240,
                emit: true,
            }
        );
        assert!(
            delta_len * 8 < keyframe_len,
            "one dirty tile ({delta_len} B) must be far smaller than the keyframe ({keyframe_len} B)"
        );
        let image = image.expect("a delta decodes against the established canvas");
        assert!(
            image.get_pixel(70, 40).0[0] > 200,
            "the repainted tile arrived"
        );
        assert!(
            image.get_pixel(10, 10).0[0].abs_diff(busy_shade(10, 10)) <= 12,
            "untouched tiles keep their established pixels"
        );
    }

    #[test]
    fn partial_edge_tiles_round_trip() {
        // 70x37 is deliberately not a multiple of the 16-pixel tile.
        let mut encoder = TileEncoder::new();
        let mut decoder = TileDecoder::new();
        let first = flat(70, 37, 30);
        roundtrip(
            &mut encoder,
            &mut decoder,
            &first,
            95,
            TileEncodeRequest::Delta,
        );

        let mut second = flat(70, 37, 30);
        // Touch the bottom-right partial tile only.
        for y in 32..37 {
            for x in 64..70 {
                second.image.put_pixel(x, y, Rgb([240, 240, 240]));
            }
        }
        let (plan, _, image) = roundtrip(
            &mut encoder,
            &mut decoder,
            &second,
            95,
            TileEncodeRequest::Delta,
        );
        assert!(plan.emit && !plan.keyframe);
        assert_eq!(plan.dirty_tiles, 1);
        let image = image.expect("edge delta decodes");
        assert_eq!(image.dimensions(), (70, 37));
        assert!(image.get_pixel(69, 36).0[0] > 200);
        assert!(image.get_pixel(0, 0).0[0] < 60);
    }

    #[test]
    fn tolerance_does_not_accumulate_against_the_transmitted_reference() {
        let mut encoder = TileEncoder::with_settings(DEFAULT_TILE_LOG2, 4);
        let mut decoder = TileDecoder::new();
        let mut value = 100_u8;
        let source = flat(64, 64, value);
        roundtrip(
            &mut encoder,
            &mut decoder,
            &source,
            95,
            TileEncodeRequest::Delta,
        );

        // Drift by one unit per frame. Each step is inside the tolerance, but
        // the comparison is against the last transmitted pixels, so the drift
        // must eventually cross it and be retransmitted.
        let mut retransmitted = false;
        for _ in 0..8 {
            value += 1;
            let next = flat(64, 64, value);
            let mut payload = Vec::new();
            let plan = encode(
                &mut encoder,
                &mut payload,
                1,
                &next,
                95,
                TileEncodeRequest::Delta,
            );
            if plan.emit {
                decoder
                    .decode_into(&payload, new_decode_buffer_pool())
                    .unwrap();
                encoder.commit(&next);
                retransmitted = true;
            }
        }
        assert!(
            retransmitted,
            "slow drift must cross the tolerance against the transmitted reference"
        );
        let reference = encoder.reference().expect("a committed encoder has one");
        assert!(reference.get_pixel(0, 0).0[0].abs_diff(value) <= 4);
    }

    #[test]
    fn an_uncommitted_frame_never_advances_the_reference() {
        let mut encoder = TileEncoder::new();
        let first = flat(64, 64, 10);
        let mut payload = Vec::new();
        encode(
            &mut encoder,
            &mut payload,
            0,
            &first,
            90,
            TileEncodeRequest::Delta,
        );
        encoder.commit(&first);

        let second = flat(64, 64, 200);
        encode(
            &mut encoder,
            &mut payload,
            1,
            &second,
            90,
            TileEncodeRequest::Delta,
        );
        encoder.discard();
        assert_eq!(
            encoder.reference().unwrap().get_pixel(0, 0).0[0],
            10,
            "a discarded frame must leave the transmitted reference untouched"
        );

        // The next encode must therefore still report the tile as dirty.
        let plan = encode(
            &mut encoder,
            &mut payload,
            2,
            &second,
            90,
            TileEncodeRequest::Delta,
        );
        assert!(plan.emit && !plan.keyframe && plan.dirty_tiles > 0);
    }

    #[test]
    fn a_resize_forces_a_keyframe() {
        let mut encoder = TileEncoder::new();
        let mut decoder = TileDecoder::new();
        let first = flat(64, 64, 10);
        roundtrip(
            &mut encoder,
            &mut decoder,
            &first,
            90,
            TileEncodeRequest::Delta,
        );

        let resized = flat(96, 64, 10);
        let (plan, _, image) = roundtrip(
            &mut encoder,
            &mut decoder,
            &resized,
            90,
            TileEncodeRequest::Delta,
        );
        assert!(plan.emit && plan.keyframe);
        assert_eq!(image.unwrap().dimensions(), (96, 64));
    }

    #[test]
    fn a_source_geometry_change_forces_a_keyframe() {
        let mut encoder = TileEncoder::new();
        let first = flat(64, 64, 10);
        let mut payload = Vec::new();
        encode(
            &mut encoder,
            &mut payload,
            0,
            &first,
            90,
            TileEncodeRequest::Delta,
        );
        encoder.commit(&first);

        let mut rotated = flat(64, 64, 10);
        rotated.source_width = 999;
        let plan = encode(
            &mut encoder,
            &mut payload,
            1,
            &rotated,
            90,
            TileEncodeRequest::Delta,
        );
        assert!(
            plan.emit && plan.keyframe,
            "a changed root geometry cannot be coded against the old reference"
        );
    }

    #[test]
    fn a_delta_without_an_established_canvas_is_rejected() {
        let mut encoder = TileEncoder::new();
        let mut decoder = TileDecoder::new();
        let first = flat(64, 64, 10);
        let mut payload = Vec::new();
        encode(
            &mut encoder,
            &mut payload,
            0,
            &first,
            90,
            TileEncodeRequest::Delta,
        );
        encoder.commit(&first);

        let mut second = flat(64, 64, 10);
        second.image.put_pixel(1, 1, Rgb([255, 255, 255]));
        encode(
            &mut encoder,
            &mut payload,
            1,
            &second,
            90,
            TileEncodeRequest::Delta,
        );
        // The keyframe never reached this decoder.
        assert!(
            decoder
                .decode_into(&payload, new_decode_buffer_pool())
                .is_err(),
            "a delta must not be applied to an absent canvas"
        );
        assert!(decoder.canvas().is_none());
    }

    #[test]
    fn malformed_headers_are_rejected_before_any_canvas_change() {
        let mut encoder = TileEncoder::new();
        let mut decoder = TileDecoder::new();
        let source = flat(64, 64, 77);
        let mut payload = Vec::new();
        encode(
            &mut encoder,
            &mut payload,
            0,
            &source,
            90,
            TileEncodeRequest::Delta,
        );
        decoder
            .decode_into(&payload, new_decode_buffer_pool())
            .unwrap();
        let baseline = decoder.canvas().unwrap().clone();

        let cases: Vec<(&str, Box<dyn Fn(&mut Vec<u8>)>)> = vec![
            (
                "truncated header",
                Box::new(|p: &mut Vec<u8>| p.truncate(TILE_HEADER_LEN - 1)),
            ),
            ("unknown flags", Box::new(|p: &mut Vec<u8>| p[17] |= 0x80)),
            (
                "unsupported tile size",
                Box::new(|p: &mut Vec<u8>| p[16] = 2),
            ),
            (
                "dirty count disagrees with bitmap",
                Box::new(|p: &mut Vec<u8>| p[23] = p[23].wrapping_sub(1)),
            ),
            (
                "dirty count above the grid",
                Box::new(|p: &mut Vec<u8>| p[20..24].copy_from_slice(&9999_u32.to_be_bytes())),
            ),
            (
                "zero atlas columns",
                Box::new(|p: &mut Vec<u8>| p[18..20].copy_from_slice(&0_u16.to_be_bytes())),
            ),
            (
                "missing atlas body",
                Box::new(|p: &mut Vec<u8>| p.truncate(TILE_HEADER_LEN + 2)),
            ),
        ];
        for (label, corrupt) in cases {
            let mut corrupted = payload.clone();
            corrupt(&mut corrupted);
            assert!(
                decoder
                    .decode_into(&corrupted, new_decode_buffer_pool())
                    .is_err(),
                "{label} must be rejected"
            );
            assert_eq!(
                decoder.canvas().unwrap().as_raw(),
                baseline.as_raw(),
                "{label} must not disturb the established canvas"
            );
        }
    }

    #[test]
    fn a_scattered_multi_tile_delta_reconstructs_every_tile() {
        let mut encoder = TileEncoder::new();
        let mut decoder = TileDecoder::new();
        let first = flat(256, 160, 20);
        roundtrip(
            &mut encoder,
            &mut decoder,
            &first,
            92,
            TileEncodeRequest::Delta,
        );

        // Dirty a diagonal so the atlas packs non-adjacent tiles.
        let mut second = flat(256, 160, 20);
        let marks: Vec<(u32, u32)> = (0..10).map(|i| (i * 16, (i % 10) * 16)).collect();
        for (index, (tx, ty)) in marks.iter().enumerate() {
            let shade = 60 + index as u8 * 18;
            for y in *ty..(*ty + 16).min(160) {
                for x in *tx..(*tx + 16).min(256) {
                    second.image.put_pixel(x, y, Rgb([shade, shade, shade]));
                }
            }
        }
        let (plan, _, image) = roundtrip(
            &mut encoder,
            &mut decoder,
            &second,
            92,
            TileEncodeRequest::Delta,
        );
        assert!(plan.emit);
        assert_eq!(plan.dirty_tiles, marks.len() as u32);
        let image = image.expect("multi-tile delta decodes");
        for (index, (tx, ty)) in marks.iter().enumerate() {
            let shade = 60 + index as u8 * 18;
            let got = image.get_pixel(tx + 8, ty + 8).0[0];
            assert!(
                got.abs_diff(shade) <= 6,
                "tile {index} at ({tx},{ty}) decoded {got}, expected ~{shade}"
            );
        }
        assert!(
            image.get_pixel(250, 155).0[0].abs_diff(20) <= 6,
            "untouched tiles keep their pixels"
        );
    }

    #[test]
    fn bitmap_tail_bits_beyond_the_grid_stay_clear() {
        // 20 tiles needs three bytes; the top four bits of the last must be 0.
        let mut bitmap = vec![0xff_u8; bitmap_len(20)];
        trim_bitmap_tail(&mut bitmap, 20);
        assert_eq!(bitmap, vec![0xff, 0xff, 0x0f]);
        let counted: u32 = bitmap.iter().map(|byte| byte.count_ones()).sum();
        assert_eq!(counted, 20);
    }

    #[test]
    fn atlas_columns_stay_within_the_protocol_dimension_limit() {
        for tile_log2 in MIN_TILE_LOG2..=MAX_TILE_LOG2 {
            let tile = 1_usize << tile_log2;
            for dirty in [1_usize, 2, 9, 100, 5000, 300_000] {
                let columns = atlas_columns(dirty, tile);
                assert!(columns >= 1 && columns <= dirty.max(1));
                assert!(
                    columns * tile <= usize::from(MAX_DIMENSION),
                    "{dirty} dirty tiles at tile {tile} produced {columns} columns"
                );
                let rows = dirty.div_ceil(columns);
                let limit = max_atlas_columns(tile);
                if dirty <= limit * limit {
                    assert!(
                        rows * tile <= usize::from(MAX_DIMENSION),
                        "{dirty} dirty tiles at tile {tile} produced {rows} rows"
                    );
                }
            }
            // No image inside the protocol pixel budget can produce more tiles
            // than a square atlas holds, so the unrepresentable branch above is
            // unreachable in practice rather than merely untested.
            let limit = max_atlas_columns(tile);
            assert!(
                (limit * limit) as u64 * (tile * tile) as u64 > MAX_PIXELS,
                "tile {tile}: a square atlas must outreach the largest legal frame"
            );
            {}
        }
    }
}
