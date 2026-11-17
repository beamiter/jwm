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
//! geometry_epoch u64
//! [base_sequence u64, scroll_dy i16, top u16, bottom u16] only when FLAG_SCROLL_COPY is set
//! bitmap         ceil(tiles_across * tiles_down / 8) bytes, raster order
//! atlas          JPEG, present only when dirty_tiles > 0
//! ```

use super::RemoteResult;
use super::frame::{
    BoundedPayloadWriter, DamageRegions, FrameDamage, MAX_DIMENSION, RecyclableDecodedFrame,
    SharedDecodeBufferPool, TileSnapshot, invalid_data, validate_dimensions,
};
use super::source_wire::MAX_FRAME_INNER_PAYLOAD_LEN;
use super::x11_capture::CapturedFrame;
use image::codecs::jpeg::{JpegDecoder, JpegEncoder};
use image::{ColorType, DynamicImage, ExtendedColorType, ImageDecoder, Limits, RgbImage};
// Existing offline benchmark children use the crate hash dependency.
#[cfg(test)]
use sha2::{Digest, Sha256};
use std::io::Cursor;
use std::time::{Duration, Instant};

/// Byte length of the fixed tile-frame header preceding the dirty bitmap.
pub(crate) const TILE_HEADER_LEN: usize = 32;

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
const FLAG_SCROLL_COPY: u8 = 1 << 1;
const FLAG_LOSSLESS: u8 = 1 << 2;
const KNOWN_FLAGS: u8 = FLAG_KEYFRAME | FLAG_SCROLL_COPY | FLAG_LOSSLESS;
const SCROLL_HEADER_LEN: usize = 14;
const MAX_SCROLL_DY: i16 = 64;

#[derive(Clone, Copy, Debug)]
struct ScrollCopy {
    base_sequence: u64,
    /// Destination row y reads old row y + dy.
    dy: i16,
    top: u16,
    bottom: u16,
}

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
    /// Explicit idle-only exact repair; dynamic captures remain JPEG.
    LosslessRepair,
    /// Send a frame even with nothing dirty, to keep the session's video
    /// liveness timer fed. An empty tile frame is a few dozen bytes.
    Keepalive,
    /// A new native input proof must get a fresh record even if identical.
    /// Use exact raw damage, not ordinary video's small-change tolerance.
    FreshProof,
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
    lossless: bool,
    scroll: Option<ScrollCopy>,
    geometry_epoch: u64,
    keyframe: bool,
    refinement_cursor: Option<usize>,
    dirty_tiles: u32,
    encoded: (u16, u16),
    source: (u16, u16),
    columns: usize,
    /// `None` for an empty keepalive frame, which carries no atlas.
    atlas: Option<(usize, u16, u16)>,
}

/// One capture's pixel damage, reusable while the quality controller changes epoch.
#[derive(Debug, Clone, Copy)]
struct DetectedFrame {
    observed_at: Instant,
    scroll: Option<ScrollCopy>,
    geometry_epoch: u64,
    keyframe: bool,
    request: TileEncodeRequest,
    encoded: (u16, u16),
    source: (u16, u16),
    total_tiles: u32,
}

/// Non-overlapping costs for the atlas of the latest successful encode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct EncodeStageTimings {
    pub(crate) atlas_pack: Duration,
    pub(crate) jpeg: Duration,
    /// Present only for a PNG atlas; `jpeg` is then zero.
    pub(crate) png: Option<Duration>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct ExactCaptureIdentity {
    generation: u64,
    geometry_epoch: u64,
    source: (u16, u16),
}

/// Host-side delta encoder holding the host's model of the viewer's canvas.
#[derive(Debug)]
pub(crate) struct TileEncoder {
    lossless_enabled: bool,
    exact: Vec<bool>,
    // Proof for the successfully committed decoded canvas, never the latest
    // observed capture. Detection may invalidate `exact` before a scroll.
    committed_exact: Vec<bool>,
    stable_since: Vec<Instant>,
    observed: Option<RgbImage>,
    observation_generation: u64,
    pending_exact_identity: Option<ExactCaptureIdentity>,
    exact_changed: bool,
    lossless_rejected: bool,
    pending_lossless: bool,
    scroll_copy_enabled: bool,
    last_sequence: Option<u64>,
    pending_sequence: Option<u64>,
    pending_scroll: Option<ScrollCopy>,
    geometry_epoch: u64,
    tile_log2: u8,
    tolerance: u8,
    /// Pixels the viewer has been sent.  `None` until the first commit.
    reference: Option<RgbImage>,
    source: (u16, u16),
    /// JPEG quality of each successfully transmitted tile, not captured pixels.
    qualities: Vec<u8>,
    pending_quality: u8,
    refinement_cursor: usize,
    pending_refinement_cursor: Option<usize>,
    grid: (usize, usize),
    bitmap: Vec<u8>,
    dirty: Vec<u32>,
    atlas: Vec<u8>,
    encode_timings: Option<EncodeStageTimings>,
    /// Dirty set of the last successful encode, awaiting commit or discard.
    pending: bool,
    planned: Option<PlannedFrame>,
    detected: Option<DetectedFrame>,
    /// Ordinary pixel damage only, never quality repairs.
    detected_dirty: Vec<u32>,
    #[cfg(test)]
    pub(crate) detection_scans: usize,
    /// Per-instance fault fixtures, absent from production. Lose detection on
    /// the selected (one-based) refinement, or fail the real JPEG writer early.
    #[cfg(test)]
    pub(crate) forget_detection_on_refinement: Option<usize>,
    #[cfg(test)]
    pub(crate) jpeg_payload_limit: Option<usize>,
}

impl TileEncoder {
    pub(crate) fn new() -> Self {
        Self::with_settings(DEFAULT_TILE_LOG2, DEFAULT_TILE_TOLERANCE)
    }

    pub(crate) fn with_settings(tile_log2: u8, tolerance: u8) -> Self {
        Self {
            lossless_enabled: false,
            exact: Vec::new(),
            committed_exact: Vec::new(),
            stable_since: Vec::new(),
            observed: None,
            observation_generation: 0,
            pending_exact_identity: None,
            exact_changed: false,
            lossless_rejected: false,
            pending_lossless: false,
            scroll_copy_enabled: false,
            last_sequence: None,
            pending_sequence: None,
            pending_scroll: None,
            geometry_epoch: 0,
            tile_log2: tile_log2.clamp(MIN_TILE_LOG2, MAX_TILE_LOG2),
            tolerance,
            reference: None,
            source: (0, 0),
            qualities: Vec::new(),
            pending_quality: 0,
            refinement_cursor: 0,
            pending_refinement_cursor: None,
            grid: (0, 0),
            bitmap: Vec::new(),
            dirty: Vec::new(),
            atlas: Vec::new(),
            encode_timings: None,
            pending: false,
            planned: None,
            detected: None,
            detected_dirty: Vec::new(),
            #[cfg(test)]
            detection_scans: 0,
            #[cfg(test)]
            forget_detection_on_refinement: None,
            #[cfg(test)]
            jpeg_payload_limit: None,
        }
    }

    /// Explicit negotiated codec permission. Changing it requires a fresh keyframe.
    pub(crate) fn set_scroll_copy_enabled(&mut self, enabled: bool) {
        if self.scroll_copy_enabled != enabled {
            self.discard();
            self.reference = None;
            self.last_sequence = None;
            self.qualities.clear();
            self.committed_exact.clear();
            self.scroll_copy_enabled = enabled;
        }
    }

    pub(crate) fn set_lossless_enabled(&mut self, enabled: bool) {
        if self.lossless_enabled != enabled {
            self.discard();
            self.exact.clear();
            self.committed_exact.clear();
            self.stable_since.clear();
            self.observed = None;
            self.lossless_enabled = enabled;
        }
    }

    #[cfg(test)]
    pub(crate) fn has_exact_debt(&self) -> bool {
        self.lossless_enabled && self.exact.iter().any(|exact| !exact)
    }

    pub(crate) fn has_stable_exact_debt(&self, now: Instant) -> bool {
        self.lossless_enabled
            && self
                .exact
                .iter()
                .zip(&self.stable_since)
                .any(|(exact, since)| {
                    !exact && now.saturating_duration_since(*since) >= Duration::from_secs(2)
                })
    }

    pub(crate) fn detected_exact_change(&self) -> bool {
        self.exact_changed
    }
    pub(crate) fn lossless_encode_rejected(&self) -> bool {
        self.lossless_rejected
    }

    /// Whether an encoded, uncommitted PNG still describes the selected pixels
    /// of a newer capture. Unselected changes do not supersede a region repair.
    /// This is read-only: commit must still use the original immutable capture,
    /// and the caller must separately validate the capture's source identity.
    pub(crate) fn pending_lossless_matches(&self, newer: &CapturedFrame) -> bool {
        if !self.pending || !self.pending_lossless || self.dirty.is_empty() {
            return false;
        }
        let (Some(identity), Some(encoded)) = (self.pending_exact_identity, &self.observed) else {
            return false;
        };
        if identity.generation != self.observation_generation
            || identity.geometry_epoch != newer.geometry_epoch
            || identity.source != (newer.source_width, newer.source_height)
            || encoded.dimensions() != newer.image.dimensions()
        {
            return false;
        }
        let width = encoded.width() as usize;
        let height = encoded.height() as usize;
        let stride = width * 3;
        let original = encoded.as_raw();
        let latest = newer.image.as_raw();
        self.dirty.iter().all(|&index| {
            let (x0, y0, x1, y1) =
                tile_bounds(index as usize, self.grid.0, self.tile(), width, height);
            (y0..y1).all(|y| {
                let start = y * stride + x0 * 3;
                let end = y * stride + x1 * 3;
                original[start..end] == latest[start..end]
            })
        })
    }

    fn tile(&self) -> usize {
        1_usize << self.tile_log2
    }

    /// True when the next frame must be a keyframe.
    fn needs_keyframe(&self, frame: &CapturedFrame) -> bool {
        let Some(reference) = self.reference.as_ref() else {
            return true;
        };
        self.geometry_epoch != frame.geometry_epoch
            || reference.dimensions() != frame.image.dimensions()
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
        self.plan_at(frame, request, Instant::now())
    }

    pub(crate) fn plan_at(
        &mut self,
        frame: &CapturedFrame,
        request: TileEncodeRequest,
        now: Instant,
    ) -> RemoteResult<TilePlan> {
        self.detect(frame, request, now)?;
        self.select_refinement(None)
    }

    /// Test convenience for the complete detect/select operation.
    #[cfg(test)]
    pub(crate) fn plan_with_refinement(
        &mut self,
        frame: &CapturedFrame,
        request: TileEncodeRequest,
        refinement: Option<(u8, usize)>,
    ) -> RemoteResult<TilePlan> {
        self.detect(frame, request, Instant::now())?;
        self.select_refinement(refinement)
    }

    fn detect(
        &mut self,
        frame: &CapturedFrame,
        request: TileEncodeRequest,
        now: Instant,
    ) -> RemoteResult<()> {
        self.pending_lossless = false;
        self.pending_exact_identity = None;
        self.pending_scroll = None;
        self.pending_sequence = None;
        self.detected = None;
        self.planned = None;
        self.pending = false;
        self.pending_refinement_cursor = None;
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
        self.exact_changed = false;
        if self.lossless_enabled {
            self.observation_generation = self
                .observation_generation
                .checked_add(1)
                .ok_or_else(|| invalid_data("exact capture generation exhausted"))?;
            if keyframe
                || self
                    .observed
                    .as_ref()
                    .is_none_or(|old| old.dimensions() != frame.image.dimensions())
            {
                self.exact = vec![false; total_tiles as usize];
                self.stable_since = vec![now; total_tiles as usize];
                self.exact_changed = true;
            } else {
                let mut changed = Vec::new();
                let mut bits = vec![0; self.bitmap.len()];
                collect_dirty(
                    &frame.image,
                    self.observed.as_ref().unwrap(),
                    self.grid,
                    tile,
                    0,
                    &mut bits,
                    &mut changed,
                );
                self.exact_changed = !changed.is_empty();
                for index in changed {
                    self.exact[index as usize] = false;
                    self.stable_since[index as usize] = now;
                }
            }
            self.observed = Some(frame.image.clone());
        }
        if keyframe {
            self.bitmap.iter_mut().for_each(|byte| *byte = 0xff);
            trim_bitmap_tail(&mut self.bitmap, total_tiles);
            self.dirty.extend(0..total_tiles);
        } else {
            let reference = self
                .reference
                .as_ref()
                .expect("a non-keyframe always has a reference image");
            #[cfg(test)]
            {
                self.detection_scans += 1;
            }
            collect_dirty(
                &frame.image,
                reference,
                (grid_w, grid_h),
                tile,
                if request == TileEncodeRequest::FreshProof {
                    0
                } else {
                    self.tolerance
                },
                &mut self.bitmap,
                &mut self.dirty,
            );
        }

        let mut scroll = None;
        if !keyframe && self.scroll_copy_enabled && self.dirty.len() * 2 > total_tiles as usize {
            if let (Some(reference), Some(base_sequence)) = (&self.reference, self.last_sequence) {
                if let Some(mut copy) = find_scroll_copy(&frame.image, reference) {
                    copy.base_sequence = base_sequence;
                    let (dest, _, rows) = scroll_ranges(copy);
                    let revised: Vec<u32> = (0..total_tiles)
                        .filter(|index| {
                            let (_, y0, _, y1) = tile_bounds(
                                *index as usize,
                                grid_w,
                                tile,
                                usize::from(encoded_width),
                                usize::from(encoded_height),
                            );
                            // Only wholly proven copied tiles can leave the normal dirty set.
                            scroll_exposes_tile(y0, y1, copy)
                                || ((y0 < dest || y1 > dest + rows)
                                    && self.bitmap[*index as usize / 8] & (1 << (*index % 8)) != 0)
                        })
                        .collect();
                    if revised.len() < self.dirty.len() {
                        self.dirty = revised;
                        self.bitmap.fill(0);
                        for index in &self.dirty {
                            self.bitmap[*index as usize / 8] |= 1 << (*index % 8);
                        }
                        scroll = Some(copy);
                    }
                }
            }
        }
        if scroll.is_some() {
            self.exact.fill(false);
            self.stable_since.fill(now);
        }
        self.detected_dirty.clone_from(&self.dirty);
        self.detected = Some(DetectedFrame {
            observed_at: now,
            scroll,
            geometry_epoch: frame.geometry_epoch,
            keyframe,
            request,
            encoded: (encoded_width, encoded_height),
            source: (frame.source_width, frame.source_height),
            total_tiles,
        });
        Ok(())
    }

    pub(crate) fn detected_dirty_tiles(&self) -> usize {
        self.detected_dirty.len()
    }

    /// Replace quality repairs without scanning the capture again. The caller
    /// must keep using the capture passed to plan until encode or discard.
    pub(crate) fn select_refinement(
        &mut self,
        refinement: Option<(u8, usize)>,
    ) -> RemoteResult<TilePlan> {
        #[cfg(test)]
        if refinement.is_some()
            && let Some(remaining) = self.forget_detection_on_refinement.as_mut()
        {
            *remaining = remaining.saturating_sub(1);
            if *remaining == 0 {
                self.detected = None;
                self.forget_detection_on_refinement = None;
            }
        }
        let detected = self
            .detected
            .ok_or_else(|| invalid_data("tile refinement selected without detection"))?;
        let DetectedFrame {
            observed_at,
            mut scroll,
            geometry_epoch,
            keyframe,
            request,
            encoded: (encoded_width, encoded_height),
            source,
            total_tiles,
        } = detected;
        self.planned = None;
        self.dirty.clone_from(&self.detected_dirty);
        self.bitmap.fill(0);
        for &index in &self.dirty {
            self.bitmap[index as usize / 8] |= 1 << (index % 8);
        }
        let tile = self.tile();
        let grid_w = self.grid.0;
        let mut refinement_cursor = keyframe.then_some(0);
        if !keyframe && let Some((quality, limit)) = refinement {
            let moved_qualities = scroll.map(|copy| {
                shifted_qualities(
                    &self.qualities,
                    self.grid,
                    usize::from(encoded_height),
                    tile,
                    copy,
                )
            });
            let (indices, next) = refinement_tiles(
                moved_qualities.as_deref().unwrap_or(&self.qualities),
                quality,
                limit,
                self.refinement_cursor,
            );
            if !indices.is_empty() {
                refinement_cursor = Some(next);
            }
            for index in indices {
                let mask = 1 << (index % 8);
                if self.bitmap[index / 8] & mask == 0 {
                    self.bitmap[index / 8] |= mask;
                    self.dirty.push(index as u32);
                }
            }
            // The bitmap decoder consumes atlas tiles in raster order. The
            // round-robin repair selection may precede ordinary damage.
            self.dirty.sort_unstable();
        }

        let lossless = self.lossless_enabled
            && request == TileEncodeRequest::LosslessRepair
            && !keyframe
            && self.detected_dirty.is_empty()
            && scroll.is_none();
        if lossless {
            // Single-column packing has no unused cells, so the actual padded
            // atlas, not merely selected source pixels, is strictly bounded.
            let limit = (65_536 / (tile * tile)).min(usize::from(MAX_DIMENSION) / tile);
            let debts: Vec<u8> = self
                .exact
                .iter()
                .zip(&self.stable_since)
                .map(|(exact, since)| {
                    u8::from(
                        *exact
                            || observed_at.saturating_duration_since(*since)
                                < Duration::from_secs(2),
                    )
                })
                .collect();
            let (indices, next) = refinement_tiles(&debts, 1, limit, self.refinement_cursor);
            self.dirty = indices.into_iter().map(|i| i as u32).collect();
            self.bitmap.fill(0);
            for index in &self.dirty {
                self.bitmap[*index as usize / 8] |= 1 << (*index % 8);
            }
            refinement_cursor = Some(next);
        }

        let dirty_tiles = u32::try_from(self.dirty.len())
            .map_err(|_| invalid_data("dirty tile count exceeds protocol range"))?;
        // A full repair carries every pixel anyway; do not charge an unused
        // copy extension or bind the receiver to an unnecessary predecessor.
        if dirty_tiles == total_tiles {
            scroll = None;
        }
        let emit = dirty_tiles > 0
            || matches!(
                request,
                TileEncodeRequest::Keepalive | TileEncodeRequest::FreshProof
            );
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
            let columns = if lossless {
                1
            } else if keyframe {
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
            lossless,
            scroll,
            geometry_epoch,
            keyframe,
            refinement_cursor,
            dirty_tiles,
            encoded: (encoded_width, encoded_height),
            source,
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
        self.encode_with_clock(payload, sequence, frame, quality, Instant::now)
    }

    /// None for failed encodes and keepalives, which perform neither stage.
    pub(crate) fn encode_timings(&self) -> Option<EncodeStageTimings> {
        self.encode_timings
    }

    fn encode_with_clock(
        &mut self,
        payload: &mut Vec<u8>,
        sequence: u64,
        frame: &CapturedFrame,
        quality: u8,
        mut now: impl FnMut() -> Instant,
    ) -> RemoteResult<()> {
        self.encode_timings = None;
        self.lossless_rejected = false;
        self.detected = None;
        payload.clear();
        let result = self.encode_inner(payload, sequence, frame, quality, &mut now);
        if result.is_err() {
            payload.clear();
            self.pending = false;
            self.pending_scroll = None;
            self.pending_sequence = None;
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
        now: &mut impl FnMut() -> Instant,
    ) -> RemoteResult<()> {
        let planned = self
            .planned
            .ok_or_else(|| invalid_data("tile frame was encoded without a plan"))?;
        let encoded = (
            u16::try_from(frame.image.width()).unwrap_or(0),
            u16::try_from(frame.image.height()).unwrap_or(0),
        );
        if planned.geometry_epoch != frame.geometry_epoch
            || planned.encoded != encoded
            || planned.source != (frame.source_width, frame.source_height)
            || (self.lossless_enabled && self.observed.as_ref() != Some(&frame.image))
        {
            return Err(invalid_data("tile frame does not match its plan").into());
        }

        payload.extend_from_slice(&sequence.to_be_bytes());
        payload.extend_from_slice(&frame.source_width.to_be_bytes());
        payload.extend_from_slice(&frame.source_height.to_be_bytes());
        payload.extend_from_slice(&planned.encoded.0.to_be_bytes());
        payload.extend_from_slice(&planned.encoded.1.to_be_bytes());
        payload.push(self.tile_log2);
        payload.push(
            if planned.keyframe { FLAG_KEYFRAME } else { 0 }
                | if planned.lossless { FLAG_LOSSLESS } else { 0 }
                | if planned.scroll.is_some() {
                    FLAG_SCROLL_COPY
                } else {
                    0
                },
        );
        payload.extend_from_slice(
            &u16::try_from(planned.columns)
                .map_err(|_| invalid_data("tile atlas column count exceeds protocol range"))?
                .to_be_bytes(),
        );
        payload.extend_from_slice(&planned.dirty_tiles.to_be_bytes());
        payload.extend_from_slice(&frame.geometry_epoch.to_be_bytes());
        if let Some(copy) = planned.scroll {
            if sequence <= copy.base_sequence {
                return Err(invalid_data("scroll sequence must advance").into());
            }
            payload.extend_from_slice(&copy.base_sequence.to_be_bytes());
            payload.extend_from_slice(&copy.dy.to_be_bytes());
            payload.extend_from_slice(&copy.top.to_be_bytes());
            payload.extend_from_slice(&copy.bottom.to_be_bytes());
        }
        payload.extend_from_slice(&self.bitmap);
        debug_assert_eq!(
            payload.len(),
            TILE_HEADER_LEN
                + if planned.scroll.is_some() {
                    SCROLL_HEADER_LEN
                } else {
                    0
                }
                + self.bitmap.len()
        );

        if let Some((rows, atlas_width, atlas_height)) = planned.atlas {
            let pack_started = now();
            self.pack_atlas(&frame.image, planned.columns, rows);
            let jpeg_started = now();
            let payload_limit = MAX_FRAME_INNER_PAYLOAD_LEN;
            #[cfg(test)]
            let payload_limit = self.jpeg_payload_limit.unwrap_or(payload_limit);
            if planned.lossless {
                match super::png_atlas::encode_rgb8(
                    u32::from(atlas_width),
                    u32::from(atlas_height),
                    &self.atlas,
                ) {
                    Ok(encoded) => payload.extend_from_slice(&encoded),
                    Err(error) => {
                        self.lossless_rejected = true;
                        return Err(error.into());
                    }
                }
            } else {
                JpegEncoder::new_with_quality(
                    BoundedPayloadWriter::new(payload, payload_limit),
                    quality.clamp(1, 100),
                )
                .encode(
                    &self.atlas,
                    u32::from(atlas_width),
                    u32::from(atlas_height),
                    ExtendedColorType::Rgb8,
                )?;
            }
            let codec_elapsed = now().saturating_duration_since(jpeg_started);
            self.encode_timings = Some(EncodeStageTimings {
                atlas_pack: jpeg_started.saturating_duration_since(pack_started),
                jpeg: if planned.lossless {
                    Duration::ZERO
                } else {
                    codec_elapsed
                },
                png: planned.lossless.then_some(codec_elapsed),
            });
        }

        self.pending_lossless = planned.lossless;
        // Reuse the owned detection snapshot for every codec. Full byte
        // equality at encode and commit binds the transaction without a
        // redundant cryptographic digest. Detection invalidates pending state
        // and advances this generation before changing the owned snapshot.
        self.pending_exact_identity = self.lossless_enabled.then_some(ExactCaptureIdentity {
            generation: self.observation_generation,
            geometry_epoch: frame.geometry_epoch,
            source: (frame.source_width, frame.source_height),
        });
        self.pending = true;
        self.pending_sequence = Some(sequence);
        self.pending_scroll = planned.scroll;
        self.pending_quality = if planned.lossless {
            100
        } else {
            quality.clamp(1, 100)
        };
        self.pending_refinement_cursor = planned.refinement_cursor;
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
        let exact_identity = ExactCaptureIdentity {
            generation: self.observation_generation,
            geometry_epoch: frame.geometry_epoch,
            source: (frame.source_width, frame.source_height),
        };
        let exact_mismatch = self.lossless_enabled
            && (self.pending_exact_identity != Some(exact_identity)
                || self.observed.as_ref() != Some(&frame.image));
        if exact_mismatch {
            // Caller violated the immutable planned-capture contract. The
            // already-sent record cannot be rolled back: force recovery, never
            // claim current pixels exact or adopt them as the sent reference.
            self.discard();
            self.reference = None;
            self.exact.fill(false);
            self.committed_exact.clear();
            return;
        }
        self.pending = false;
        self.last_sequence = self.pending_sequence.take();
        if let Some(copy) = self.pending_scroll.take() {
            let height = frame.image.height() as usize;
            if self.lossless_enabled {
                self.exact =
                    copied_exact_tiles(&self.committed_exact, self.grid, height, self.tile(), copy);
            }
            self.qualities =
                shifted_qualities(&self.qualities, self.grid, height, self.tile(), copy);
            shift_rgb_rows(self.reference.as_mut().expect("scroll has reference"), copy);
        }
        if let Some(cursor) = self.pending_refinement_cursor.take() {
            self.refinement_cursor = cursor;
        }
        self.source = (frame.source_width, frame.source_height);
        self.geometry_epoch = frame.geometry_epoch;
        let tile = self.tile();
        let (grid_w, grid_h) = self.grid;
        self.qualities.resize(grid_w * grid_h, 0);
        if self.lossless_enabled {
            self.exact.resize(grid_w * grid_h, false);
            for index in &self.dirty {
                self.exact[*index as usize] = self.pending_lossless;
            }
            self.committed_exact.clone_from(&self.exact);
        }
        for index in &self.dirty {
            self.qualities[*index as usize] = self.pending_quality;
        }
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
        self.pending_lossless = false;
        self.pending_exact_identity = None;
        self.pending_scroll = None;
        self.pending_sequence = None;
        self.detected = None;
        self.pending = false;
        self.planned = None;
    }

    pub(crate) fn has_quality_debt(&self, quality: u8) -> bool {
        self.qualities.iter().any(|sent| *sent < quality)
    }

    #[cfg(test)]
    pub(crate) fn reference(&self) -> Option<&RgbImage> {
        self.reference.as_ref()
    }
}

/// Bounded round-robin selection avoids starving the tail while quality rises.
/// Return raster order for the wire atlas, and commit the cursor only after a
/// successful send. Planning itself never pays down debt or changes fairness.
fn refinement_tiles(
    qualities: &[u8],
    target: u8,
    limit: usize,
    cursor: usize,
) -> (Vec<usize>, usize) {
    if qualities.is_empty() || limit == 0 {
        return (Vec::new(), 0);
    }
    let start = cursor % qualities.len();
    let mut selected: Vec<_> = (start..qualities.len())
        .chain(0..start)
        .filter(|index| qualities[*index] < target)
        .take(limit)
        .collect();
    let next = selected
        .last()
        .map_or(start, |last| (last + 1) % qualities.len());
    selected.sort_unstable();
    (selected, next)
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
    lossless_enabled: bool,
    last_lossless: bool,
    scroll_copy_enabled: bool,
    tile_log2: Option<u8>,
    frame_limits: super::messages::FrameLimits,
    geometry_epoch: u64,
    snapshot: Option<TileSnapshot>,
    last_sequence: Option<u64>,
    canvas: Option<RgbImage>,
    source: (u16, u16),
    atlas: Vec<u8>,
}

impl TileDecoder {
    #[cfg(test)]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn with_frame_limits(
        frame_limits: super::messages::FrameLimits,
    ) -> RemoteResult<Self> {
        frame_limits.validate()?;
        Ok(Self {
            frame_limits,
            ..Self::default()
        })
    }

    /// Explicit negotiated codec permission. Changing it requires a fresh keyframe.
    pub(crate) fn set_scroll_copy_enabled(&mut self, enabled: bool) {
        if self.scroll_copy_enabled != enabled {
            self.scroll_copy_enabled = enabled;
            self.canvas = None;
            self.snapshot = None;
            self.last_sequence = None;
            self.tile_log2 = None;
        }
    }

    pub(crate) fn set_lossless_enabled(&mut self, enabled: bool) {
        self.lossless_enabled = enabled;
    }

    pub(crate) fn last_frame_was_lossless(&self) -> bool {
        self.last_lossless
    }

    /// Decode one authenticated tile-frame payload into a complete frame.
    pub(crate) fn decode_into(
        &mut self,
        payload: &[u8],
        pool: SharedDecodeBufferPool,
    ) -> RemoteResult<RecyclableDecodedFrame> {
        let header = TileHeader::parse(payload)?;
        if header.lossless
            && (!self.lossless_enabled
                || self.tile_log2 != Some(header.tile_log2)
                || self
                    .last_sequence
                    .is_none_or(|last| header.sequence <= last))
        {
            return Err(invalid_data("lossless atlas was not negotiated").into());
        }
        self.frame_limits
            .validate_dimensions(header.encoded_width, header.encoded_height)?;
        if header.geometry_epoch < self.geometry_epoch {
            return Err(invalid_data("video geometry epoch moved backwards").into());
        }

        let bitmap_len = bitmap_len(header.total_tiles);
        let body_start = header
            .header_len()
            .checked_add(bitmap_len)
            .ok_or_else(|| invalid_data("tile frame header length overflow"))?;
        if payload.len() < body_start {
            return Err(invalid_data("tile frame is missing its dirty bitmap").into());
        }
        let bitmap = &payload[header.header_len()..body_start];
        let counted = bitmap.iter().map(|byte| byte.count_ones()).sum::<u32>();
        if counted != header.dirty_tiles {
            return Err(
                invalid_data("tile frame bitmap does not match its declared dirty count").into(),
            );
        }
        if header.keyframe && header.dirty_tiles != header.total_tiles {
            return Err(invalid_data("tile keyframe does not carry every tile").into());
        }

        // A delta reuses the established pixels and therefore their source
        // coordinate space. Only a self-contained keyframe may change it.
        // Reject before mutating the canvas, source metadata or atlas scratch.
        if !header.keyframe
            && (self.geometry_epoch != header.geometry_epoch
                || self.source != (header.source_width, header.source_height))
        {
            return Err(invalid_data(
                "tile delta frame does not match the current source geometry",
            )
            .into());
        }

        let canvas_dimensions = (
            u32::from(header.encoded_width),
            u32::from(header.encoded_height),
        );
        if !header.keyframe
            && self
                .canvas
                .as_ref()
                .is_none_or(|canvas| canvas.dimensions() != canvas_dimensions)
        {
            return Err(
                invalid_data("tile delta frame does not match the current viewer canvas").into(),
            );
        }
        if let Some(copy) = header.scroll {
            if !self.scroll_copy_enabled
                || self.last_sequence != Some(copy.base_sequence)
                || header.sequence <= copy.base_sequence
                || self.tile_log2 != Some(header.tile_log2)
            {
                return Err(invalid_data("scroll copy capability or base mismatch").into());
            }
            let tile = 1usize << header.tile_log2;
            let grid_w = usize::from(header.encoded_width).div_ceil(tile);
            for index in 0..header.total_tiles as usize {
                let (_, y0, _, y1) = tile_bounds(
                    index,
                    grid_w,
                    tile,
                    usize::from(header.encoded_width),
                    usize::from(header.encoded_height),
                );
                if scroll_exposes_tile(y0, y1, copy) && bitmap[index / 8] & (1 << (index % 8)) == 0
                {
                    return Err(invalid_data("scroll exposed strip is not fully supplied").into());
                }
            }
        }
        // All fallible parsing and JPEG decoding precedes any canvas, snapshot
        // or scope mutation. A rejected copy cannot poison its next base.
        let atlas_layout = if header.dirty_tiles > 0 {
            let tile = 1usize << header.tile_log2;
            let columns = usize::from(header.atlas_columns);
            let dirty = header.dirty_tiles as usize;
            if columns == 0 {
                return Err(invalid_data("tile frame declares no atlas columns").into());
            }
            if columns > dirty {
                return Err(
                    invalid_data("tile frame atlas is wider than its dirty tile count").into(),
                );
            }
            let rows = dirty.div_ceil(columns);
            let width = u16::try_from(columns * tile)
                .map_err(|_| invalid_data("tile atlas width exceeds protocol range"))?;
            let height = u16::try_from(rows * tile)
                .map_err(|_| invalid_data("tile atlas height exceeds protocol range"))?;
            validate_dimensions(width, height)?;
            if payload.len() <= body_start {
                return Err(invalid_data("tile frame is missing its atlas body").into());
            }
            if header.lossless {
                super::png_atlas::decode_rgb8_into(
                    &payload[body_start..],
                    u32::from(width),
                    u32::from(height),
                    &mut self.atlas,
                )?;
            } else {
                decode_atlas(
                    &mut self.atlas,
                    &payload[body_start..],
                    width,
                    height,
                    payload.len(),
                )?;
            }
            Some((tile, columns, width))
        } else {
            if header.scroll.is_some() || header.atlas_columns != 0 || payload.len() != body_start {
                return Err(invalid_data("empty tile frame has unexpected body or copy").into());
            }
            None
        };
        if header.keyframe
            && self
                .canvas
                .as_ref()
                .is_none_or(|canvas| canvas.dimensions() != canvas_dimensions)
        {
            self.canvas = Some(RgbImage::new(canvas_dimensions.0, canvas_dimensions.1));
        }
        if header.geometry_epoch != self.geometry_epoch {
            self.snapshot = None;
            self.last_sequence = None;
        }
        self.geometry_epoch = header.geometry_epoch;
        self.source = (header.source_width, header.source_height);
        self.tile_log2 = Some(header.tile_log2);
        let canvas = self
            .canvas
            .as_mut()
            .expect("validated canvas or new keyframe");
        if let Some(copy) = header.scroll {
            shift_rgb_rows(canvas, copy);
        }
        if let Some((tile, columns, width)) = atlas_layout {
            scatter_atlas(
                canvas,
                &self.atlas,
                bitmap,
                header.total_tiles,
                tile,
                columns,
                width,
            );
        }

        let damage = if header.keyframe || header.scroll.is_some() {
            None
        } else {
            self.last_sequence.map(|base_sequence| FrameDamage {
                base_sequence,
                regions: dirty_regions(
                    bitmap,
                    header.encoded_width,
                    header.encoded_height,
                    header.tile_log2,
                ),
            })
        };
        let snapshot = TileSnapshot::update(
            canvas,
            if header.scroll.is_some() {
                None
            } else {
                self.snapshot.as_ref()
            },
            bitmap,
            1usize << header.tile_log2,
        );
        self.snapshot = Some(snapshot.clone());
        self.last_sequence = Some(header.sequence);
        self.last_lossless = header.lossless;
        Ok(RecyclableDecodedFrame::from_tiles(
            header.sequence,
            header.source_width,
            header.source_height,
            snapshot,
            pool,
        )
        .with_damage(damage)
        .with_geometry_epoch(header.geometry_epoch))
    }

    #[cfg(test)]
    pub(crate) fn canvas(&self) -> Option<&RgbImage> {
        self.canvas.as_ref()
    }
}

/// Validate the single JPEG envelope emitted by our encoder. The image crate
/// intentionally uses a tolerant entropy decoder; this marker walk rejects
/// missing/truncated envelopes and trailing data, but is not a proof that every
/// entropy-coded coefficient is semantically valid.
fn validate_atlas_jpeg_envelope(body: &[u8]) -> RemoteResult<()> {
    let malformed =
        || invalid_data("tile atlas must contain one complete JPEG without trailing data");
    if !body.starts_with(&[0xff, 0xd8]) {
        return Err(malformed().into());
    }
    let mut at = 2usize;
    let mut entropy = false;
    let mut saw_scan = false;
    loop {
        if entropy {
            while body.get(at).is_some_and(|byte| *byte != 0xff) {
                at += 1;
            }
        }
        if body.get(at) != Some(&0xff) {
            return Err(malformed().into());
        }
        while body.get(at) == Some(&0xff) {
            at += 1;
        }
        let marker = *body.get(at).ok_or_else(malformed)?;
        at += 1;
        if entropy && (marker == 0 || (0xd0..=0xd7).contains(&marker)) {
            continue;
        }
        if marker == 0xd9 {
            return if saw_scan && at == body.len() {
                Ok(())
            } else {
                Err(malformed().into())
            };
        }
        if marker == 0 || marker == 0xd8 || marker == 1 || (0xd0..=0xd7).contains(&marker) {
            return Err(malformed().into());
        }
        let length_bytes = body.get(at..at + 2).ok_or_else(malformed)?;
        let length = usize::from(u16::from_be_bytes([length_bytes[0], length_bytes[1]]));
        if length < 2 {
            return Err(malformed().into());
        }
        at = at
            .checked_add(length)
            .filter(|end| *end <= body.len())
            .ok_or_else(malformed)?;
        entropy = marker == 0xda;
        saw_scan |= entropy;
    }
}

fn decode_atlas(
    atlas: &mut Vec<u8>,
    body: &[u8],
    atlas_width: u16,
    atlas_height: u16,
    payload_len: usize,
) -> RemoteResult<()> {
    validate_atlas_jpeg_envelope(body)?;
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
            // Do not remove this initialization: the tolerant JPEG backend
            // can report success after a partial scan without filling its tail.
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
    lossless: bool,
    scroll: Option<ScrollCopy>,
    geometry_epoch: u64,
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
    fn header_len(&self) -> usize {
        TILE_HEADER_LEN
            + if self.scroll.is_some() {
                SCROLL_HEADER_LEN
            } else {
                0
            }
    }
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
        let geometry_epoch = u64::from_be_bytes(payload[24..32].try_into().unwrap());

        validate_dimensions(source_width, source_height)?;
        validate_dimensions(encoded_width, encoded_height)?;
        if !(MIN_TILE_LOG2..=MAX_TILE_LOG2).contains(&tile_log2) {
            return Err(invalid_data("tile frame declares an unsupported tile size").into());
        }
        if flags & !KNOWN_FLAGS != 0 {
            return Err(invalid_data("tile frame sets unknown flags").into());
        }
        let lossless = flags & FLAG_LOSSLESS != 0;
        if lossless && (flags & (FLAG_KEYFRAME | FLAG_SCROLL_COPY) != 0 || dirty_tiles == 0) {
            return Err(invalid_data("lossless atlas requires a nonempty plain delta").into());
        }
        let scroll = if flags & FLAG_SCROLL_COPY != 0 {
            if flags & FLAG_KEYFRAME != 0 || payload.len() < TILE_HEADER_LEN + SCROLL_HEADER_LEN {
                return Err(invalid_data("invalid scroll copy header").into());
            }
            let base_sequence = u64::from_be_bytes(payload[32..40].try_into().unwrap());
            let dy = i16::from_be_bytes(payload[40..42].try_into().unwrap());
            let top = u16::from_be_bytes(payload[42..44].try_into().unwrap());
            let bottom = u16::from_be_bytes(payload[44..46].try_into().unwrap());
            if top >= bottom
                || bottom > encoded_height
                || dy.unsigned_abs() >= bottom - top
                || (bottom - top - dy.unsigned_abs()) < encoded_height.div_ceil(2)
                || dy == 0
                || dy.unsigned_abs() > MAX_SCROLL_DY as u16
                || dy.unsigned_abs() >= encoded_height
            {
                return Err(invalid_data("scroll displacement out of range").into());
            }
            Some(ScrollCopy {
                base_sequence,
                dy,
                top,
                bottom,
            })
        } else {
            None
        };
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
            lossless,
            scroll,
            geometry_epoch,
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

/// Fixed search budget: at most 128 * 64 * 3 pixels. Choose one displacement,
/// then inspect at most one image of raw RGB for its longest exact row run.
/// A failed candidate never triggers another full-image verification.
fn find_scroll_copy(current: &RgbImage, previous: &RgbImage) -> Option<ScrollCopy> {
    if current.dimensions() != previous.dimensions() {
        return None;
    }
    let width = current.width() as usize;
    let height = current.height() as usize;
    if width == 0 || height < 2 {
        return None;
    }
    let mut best = None;
    let mut best_run = 0;
    for magnitude in 1..=(height / 2).min(MAX_SCROLL_DY as usize) {
        for dy in [magnitude as i16, -(magnitude as i16)] {
            let first = if dy < 0 { magnitude } else { 0 };
            let rows = height - magnitude;
            let count = rows.min(64);
            let mut run = 0;
            for sample in 0..count {
                let y = first + sample * rows / count;
                let source = (y as isize + dy as isize) as usize;
                let matches = [0, width / 2, width - 1].into_iter().all(|x| {
                    current.get_pixel(x as u32, y as u32)
                        == previous.get_pixel(x as u32, source as u32)
                });
                run = if matches { run + 1 } else { 0 };
                if run > best_run {
                    best_run = run;
                    best = Some(dy);
                }
            }
        }
    }
    let dy = best?;
    let amount = usize::from(dy.unsigned_abs());
    let first = if dy < 0 { amount } else { 0 };
    let stride = width * 3;
    let mut run = 0;
    let mut longest = (0, 0);
    for y in first..first + height - amount {
        let source = (y as isize + dy as isize) as usize;
        if current.as_raw()[y * stride..(y + 1) * stride]
            == previous.as_raw()[source * stride..(source + 1) * stride]
        {
            run += 1;
            if run > longest.1 {
                longest = (y + 1 - run, run);
            }
        } else {
            run = 0;
        }
    }
    if longest.1 < height.div_ceil(2) {
        return None;
    }
    let top = if dy < 0 {
        longest.0 - amount
    } else {
        longest.0
    };
    let bottom = longest.0 + longest.1 + if dy > 0 { amount } else { 0 };
    Some(ScrollCopy {
        base_sequence: 0,
        dy,
        top: top as u16,
        bottom: bottom as u16,
    })
}

fn scroll_ranges(copy: ScrollCopy) -> (usize, usize, usize) {
    let top = usize::from(copy.top);
    let amount = usize::from(copy.dy.unsigned_abs());
    let rows = usize::from(copy.bottom - copy.top) - amount;
    if copy.dy > 0 {
        (top, top + amount, rows)
    } else {
        (top + amount, top, rows)
    }
}
fn scroll_exposes_tile(y0: usize, y1: usize, copy: ScrollCopy) -> bool {
    let (dest, _, rows) = scroll_ranges(copy);
    // Every tile intersecting band but not wholly covered by the proven overlap
    // is supplied in full, including band-boundary tiles with outside pixels.
    y0 < usize::from(copy.bottom) && y1 > usize::from(copy.top) && (y0 < dest || y1 > dest + rows)
}
fn shift_rgb_rows(image: &mut RgbImage, copy: ScrollCopy) {
    let stride = image.width() as usize * 3;
    let (dest, source, rows) = scroll_ranges(copy);
    image
        .as_mut()
        .copy_within(source * stride..(source + rows) * stride, dest * stride);
}
/// Only complete destination tiles wholly inside the proven copy overlap
/// inherit exactness. Partial/band-boundary and unchanged outside tiles remain
/// conservative debt; each source tile touched by a non-aligned shift must be
/// exact in the previously committed canvas.
fn copied_exact_tiles(
    committed: &[bool],
    grid: (usize, usize),
    height: usize,
    tile: usize,
    copy: ScrollCopy,
) -> Vec<bool> {
    let mut result = vec![false; grid.0 * grid.1];
    let (dest, source, rows) = scroll_ranges(copy);
    for (index, exact) in result.iter_mut().enumerate() {
        let y0 = (index / grid.0) * tile;
        let y1 = (y0 + tile).min(height);
        if y0 < dest || y1 > dest + rows || y0 >= y1 {
            continue;
        }
        let source0 = (source + y0 - dest) / tile;
        let source1 = (source + y1 - 1 - dest) / tile;
        *exact = (source0..=source1).all(|row| {
            committed
                .get(row * grid.0 + index % grid.0)
                .copied()
                .unwrap_or(false)
        });
    }
    result
}

fn shifted_qualities(
    old: &[u8],
    grid: (usize, usize),
    height: usize,
    tile: usize,
    copy: ScrollCopy,
) -> Vec<u8> {
    let mut result = old.to_vec();
    let (dest, _, rows) = scroll_ranges(copy);
    for (index, quality) in result.iter_mut().enumerate() {
        let row = index / grid.0;
        let y0 = row * tile;
        let y1 = ((row + 1) * tile).min(height);
        if scroll_exposes_tile(y0, y1, copy) {
            *quality = 0;
            continue;
        }
        if y0 < dest || y1 > dest + rows {
            continue;
        }
        let source0 = (y0 as isize + copy.dy as isize) as usize / tile;
        let source1 = ((y1 - 1) as isize + copy.dy as isize) as usize / tile;
        *quality = (source0..=source1)
            .map(|r| old.get(r * grid.0 + index % grid.0).copied().unwrap_or(0))
            .min()
            .unwrap_or(0);
    }
    result
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

/// Bounded merged upload regions; full snapshots remain the mailbox fallback.
fn dirty_regions(bitmap: &[u8], width: u16, height: u16, tile_log2: u8) -> DamageRegions {
    let tile = 1usize << tile_log2;
    let grid_w = usize::from(width).div_ceil(tile);
    let total = grid_w * usize::from(height).div_ceil(tile);
    let mut regions = DamageRegions::default();
    for index in 0..total {
        if bitmap[index / 8] & (1 << (index % 8)) == 0 {
            continue;
        }
        let (x0, y0, x1, y1) =
            tile_bounds(index, grid_w, tile, usize::from(width), usize::from(height));
        regions.add(
            (x0 as u16, y0 as u16, (x1 - x0) as u16, (y1 - y0) as u16),
            (width, height),
        );
    }
    regions
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

    #[test]
    fn jpeg_payload_cap_reserves_source_epoch_and_padding() {
        use std::io::Write;
        assert_eq!(
            MAX_FRAME_INNER_PAYLOAD_LEN + 9 + 1,
            super::super::protocol::MAX_PAYLOAD_LEN
        );
        let mut payload = vec![0; MAX_FRAME_INNER_PAYLOAD_LEN - 1];
        let mut writer = BoundedPayloadWriter::new(&mut payload, MAX_FRAME_INNER_PAYLOAD_LEN);
        assert!(writer.write_all(&[1]).is_ok());
        assert!(writer.write_all(&[2]).is_err());
        assert_eq!(payload.len(), MAX_FRAME_INNER_PAYLOAD_LEN);
        assert_eq!(payload.last(), Some(&1));
    }

    #[test]
    fn tiny_canvas_rejects_forged_atlas_before_jpeg_allocation() {
        use crate::remote::messages::FrameLimits;
        let limits = FrameLimits {
            max_width: 1,
            max_height: 1,
            max_pixels: 1,
        };
        let mut encoder = TileEncoder::new();
        let source = flat(1, 1, 100);
        encoder.plan(&source, TileEncodeRequest::Keyframe).unwrap();
        let mut payload = Vec::new();
        encoder.encode_into(&mut payload, 1, &source, 85).unwrap();
        let mut decoder = TileDecoder::with_frame_limits(limits).unwrap();
        decoder
            .decode_into(&payload, new_decode_buffer_pool())
            .unwrap();
        assert_eq!(decoder.canvas.as_ref().unwrap().dimensions(), (1, 1));
        // One actual tile plus edge padding is allowed, not an arbitrary atlas.
        assert_eq!(
            decoder.atlas.len(),
            3 * (1 << DEFAULT_TILE_LOG2) * (1 << DEFAULT_TILE_LOG2)
        );
        for (range, bytes) in [
            (18..20, 1024_u16.to_be_bytes().to_vec()),
            (20..24, 100_u32.to_be_bytes().to_vec()),
        ] {
            let mut forged = payload.clone();
            forged[range].copy_from_slice(&bytes);
            let mut fresh = TileDecoder::with_frame_limits(limits).unwrap();
            assert!(
                fresh
                    .decode_into(&forged, new_decode_buffer_pool())
                    .is_err()
            );
            assert!(fresh.atlas.is_empty());
        }
        // Even a plausible one-tile header cannot smuggle a larger JPEG body.
        let large = flat(32, 32, 100);
        encoder.plan(&large, TileEncodeRequest::Keyframe).unwrap();
        let mut large_payload = Vec::new();
        encoder
            .encode_into(&mut large_payload, 2, &large, 85)
            .unwrap();
        let mut forged = payload[..TILE_HEADER_LEN + 1].to_vec();
        forged.extend_from_slice(&large_payload[TILE_HEADER_LEN + 1..]);
        let mut fresh = TileDecoder::with_frame_limits(limits).unwrap();
        assert!(
            fresh
                .decode_into(&forged, new_decode_buffer_pool())
                .is_err()
        );
        assert!(fresh.atlas.is_empty());
    }

    #[test]
    fn negotiated_canvas_limits_precede_allocation_and_geometry_changes() {
        use crate::remote::messages::FrameLimits;
        let limits = FrameLimits {
            max_width: 64,
            max_height: 64,
            max_pixels: 1024,
        };
        let mut decoder = TileDecoder::with_frame_limits(limits).unwrap();
        let mut encoder = TileEncoder::new();
        let mut payload = Vec::new();
        for (sequence, width, height) in [(1, 65, 1), (2, 1, 65), (3, 33, 32)] {
            let source = flat(width, height, 100);
            encoder.plan(&source, TileEncodeRequest::Keyframe).unwrap();
            encoder
                .encode_into(&mut payload, sequence, &source, 85)
                .unwrap();
            assert!(
                decoder
                    .decode_into(&payload, new_decode_buffer_pool())
                    .unwrap_err()
                    .to_string()
                    .contains("negotiated")
            );
            assert!(decoder.canvas.is_none());
            assert!(decoder.atlas.is_empty());
            assert!(decoder.snapshot.is_none());
        }
        let source = flat(32, 32, 100);
        encoder.plan(&source, TileEncodeRequest::Keyframe).unwrap();
        encoder.encode_into(&mut payload, 4, &source, 85).unwrap();
        decoder
            .decode_into(&payload, new_decode_buffer_pool())
            .unwrap();
        let old_epoch = decoder.geometry_epoch;
        let mut too_large = flat(64, 32, 100);
        too_large.geometry_epoch = old_epoch + 1;
        encoder
            .plan(&too_large, TileEncodeRequest::Keyframe)
            .unwrap();
        encoder
            .encode_into(&mut payload, 5, &too_large, 85)
            .unwrap();
        assert!(
            decoder
                .decode_into(&payload, new_decode_buffer_pool())
                .is_err()
        );
        assert_eq!(decoder.geometry_epoch, old_epoch);
        assert_eq!(decoder.canvas.as_ref().unwrap().dimensions(), (32, 32));
        assert!(
            TileDecoder::with_frame_limits(FrameLimits {
                max_pixels: 0,
                ..limits
            })
            .is_err()
        );
    }

    #[test]
    fn display_damage_bounds_cover_edge_tiles_and_empty_keepalive() {
        assert!(dirty_regions(&[0], 33, 17, 4).as_slice().is_empty());
        assert_eq!(dirty_regions(&[1], 33, 17, 4).as_slice(), &[(0, 0, 16, 16)]);
        assert_eq!(
            dirty_regions(&[32], 33, 17, 4).as_slice(),
            &[(32, 16, 1, 1)]
        );
        assert_eq!(
            dirty_regions(&[33], 33, 17, 4).as_slice(),
            &[(0, 0, 16, 16), (32, 16, 1, 1)]
        );
    }

    fn frame(width: u32, height: u32, fill: impl Fn(u32, u32) -> Rgb<u8>) -> CapturedFrame {
        let mut image = RgbImage::new(width, height);
        for (x, y, pixel) in image.enumerate_pixels_mut() {
            *pixel = fill(x, y);
        }
        CapturedFrame {
            geometry_epoch: 0,
            image,
            source_width: u16::try_from(width * 2).unwrap(),
            source_height: u16::try_from(height * 2).unwrap(),
        }
    }

    fn flat(width: u32, height: u32, value: u8) -> CapturedFrame {
        frame(width, height, |_, _| Rgb([value, value, value]))
    }

    // Six tiles: a full tile plus right/bottom/corner-clipped candidates.
    // All other tiles change below JPEG tolerance and are too young to repair.
    fn pending_region_repair(selected: usize) -> (TileEncoder, CapturedFrame, Instant) {
        let mut encoder = TileEncoder::new();
        encoder.set_lossless_enabled(true);
        let mut source = flat(33, 17, 90);
        let base = Instant::now();
        let mut payload = Vec::new();
        encoder
            .plan_at(&source, TileEncodeRequest::Keyframe, base)
            .unwrap();
        encoder.encode_into(&mut payload, 1, &source, 75).unwrap();
        assert!(!encoder.pending_lossless_matches(&source));
        encoder.commit(&source);
        for index in 0..6 {
            if index != selected {
                source
                    .image
                    .get_pixel_mut((index % 3 * 16) as u32, (index / 3 * 16) as u32)
                    .0[0] += 1;
            }
        }
        assert!(
            !encoder
                .plan_at(
                    &source,
                    TileEncodeRequest::Delta,
                    base + Duration::from_secs(1)
                )
                .unwrap()
                .emit
        );
        let now = base + Duration::from_secs(2);
        let plan = encoder
            .plan_at(&source, TileEncodeRequest::LosslessRepair, now)
            .unwrap();
        assert_eq!(plan.dirty_tiles, 1);
        assert_eq!(encoder.dirty, [selected as u32]);
        assert!(!encoder.pending_lossless_matches(&source));
        encoder.encode_into(&mut payload, 2, &source, 75).unwrap();
        assert!(encoder.pending_lossless_matches(&source));
        (encoder, source, now)
    }

    #[test]
    fn pending_lossless_matches_only_selected_rgb_including_clipped_tiles() {
        for selected in [0, 2, 3, 5] {
            let (encoder, mut newer, _) = pending_region_repair(selected);
            let reference = encoder.reference.clone();
            let observed = encoder.observed.clone();
            let exact = encoder.exact.clone();
            let cursor = encoder.refinement_cursor;
            for y in 0..17 {
                for x in 0..33 {
                    for channel in 0..3 {
                        let original = newer.image.get_pixel(x, y).0[channel];
                        let changed_tile = (y / 16 * 3 + x / 16) as usize;
                        for delta in [-1_i16, 1] {
                            newer.image.get_pixel_mut(x, y).0[channel] =
                                (i16::from(original) + delta) as u8;
                            assert_eq!(
                                encoder.pending_lossless_matches(&newer),
                                changed_tile != selected,
                                "selected={selected}, changed=({x},{y},{channel}), delta={delta}"
                            );
                        }
                        newer.image.get_pixel_mut(x, y).0[channel] = original;
                    }
                }
            }
            assert_eq!(encoder.reference, reference);
            assert_eq!(encoder.observed, observed);
            assert_eq!(encoder.exact, exact);
            assert_eq!(encoder.refinement_cursor, cursor);
            assert_eq!(encoder.last_sequence, Some(1));
            assert_eq!(encoder.pending_sequence, Some(2));
        }
    }

    #[test]
    fn pending_lossless_matching_rejects_geometry_dimensions_and_retired_candidates() {
        let (mut encoder, mut source, _) = pending_region_repair(5);
        source.geometry_epoch += 1;
        assert!(!encoder.pending_lossless_matches(&source));
        source.geometry_epoch -= 1;
        source.source_width += 1;
        assert!(!encoder.pending_lossless_matches(&source));
        source.source_width -= 1;
        source.source_height += 1;
        assert!(!encoder.pending_lossless_matches(&source));
        source.source_height -= 1;
        let original = source.image.clone();
        source.image = RgbImage::from_pixel(34, 17, Rgb([90; 3]));
        assert!(!encoder.pending_lossless_matches(&source));
        source.image = RgbImage::from_pixel(33, 18, Rgb([90; 3]));
        assert!(!encoder.pending_lossless_matches(&source));
        source.image = original;
        assert!(encoder.pending_lossless_matches(&source));
        let reference = encoder.reference.clone();
        let exact = encoder.exact.clone();
        let cursor = encoder.refinement_cursor;
        encoder.discard();
        assert!(!encoder.pending_lossless_matches(&source));
        assert_eq!(encoder.reference, reference);
        assert_eq!(encoder.exact, exact);
        assert_eq!(encoder.refinement_cursor, cursor);
        assert_eq!(encoder.last_sequence, Some(1));
        assert_eq!(encoder.pending_sequence, None);
    }

    #[test]
    fn region_repair_commit_leaves_unsent_neighbor_damage_for_next_jpeg() {
        let (mut encoder, source, now) = pending_region_repair(5);
        let mut newer = flat(33, 17, 90);
        newer.image.clone_from(&source.image);
        newer.image.get_pixel_mut(0, 0).0[0] = 130;
        assert!(encoder.pending_lossless_matches(&newer));
        // Commit the original capture, never adopt the newer mailbox image.
        encoder.commit(&source);
        assert!(!encoder.pending_lossless_matches(&source));
        assert_eq!(encoder.exact, [false, false, false, false, false, true]);
        assert_eq!(encoder.reference().unwrap().get_pixel(0, 0).0[0], 90);
        assert_eq!(encoder.observed.as_ref().unwrap(), &source.image);
        let plan = encoder
            .plan_at(&newer, TileEncodeRequest::Delta, now)
            .unwrap();
        assert_eq!(plan.dirty_tiles, 1);
        assert_eq!(encoder.dirty, [0]);
        assert!(!encoder.planned.unwrap().lossless);
        let mut payload = Vec::new();
        encoder.encode_into(&mut payload, 3, &newer, 75).unwrap();
        encoder.commit(&newer);
        assert_eq!(encoder.reference().unwrap().get_pixel(0, 0).0[0], 130);
        assert!(encoder.exact[5]);
    }

    #[test]
    fn region_match_does_not_authorize_committing_the_newer_capture() {
        let (mut encoder, mut newer, now) = pending_region_repair(5);
        newer.image.get_pixel_mut(0, 0).0[0] = 130;
        assert!(encoder.pending_lossless_matches(&newer));
        // Region matching is only a publication check. The immutable-capture
        // contract must still reject a caller substituting newer input at commit.
        encoder.commit(&newer);
        assert!(encoder.reference().is_none());
        assert!(encoder.has_exact_debt());
        assert!(
            encoder
                .plan_at(&newer, TileEncodeRequest::Delta, now)
                .unwrap()
                .keyframe
        );
    }

    #[test]
    fn lossless_tile_stability_preserves_neighbor_and_rejects_young_tolerance_change() {
        let mut source = busy(32, 16);
        let base = Instant::now();
        let mut encoder = TileEncoder::new();
        encoder.set_lossless_enabled(true);
        let mut payload = Vec::new();
        encoder
            .plan_at(&source, TileEncodeRequest::Keyframe, base)
            .unwrap();
        encoder.encode_into(&mut payload, 1, &source, 75).unwrap();
        encoder.commit(&source);
        assert!(!encoder.has_stable_exact_debt(base + Duration::from_millis(1999)));
        source.image.get_pixel_mut(0, 0).0[0] ^= 1;
        let now = base + Duration::from_secs(2);
        assert_eq!(
            encoder
                .plan_at(&source, TileEncodeRequest::Delta, now)
                .unwrap()
                .dirty_tiles,
            0
        );
        assert!(encoder.has_stable_exact_debt(now));
        let repair = encoder
            .plan_at(&source, TileEncodeRequest::LosslessRepair, now)
            .unwrap();
        assert_eq!(repair.dirty_tiles, 1);
        assert_eq!(encoder.dirty, [1]); // Only the unchanged neighbor is old enough.
        encoder.encode_into(&mut payload, 2, &source, 75).unwrap();
        encoder.discard();
        assert!(encoder.exact.iter().all(|exact| !exact));
        encoder
            .plan_at(&source, TileEncodeRequest::LosslessRepair, now)
            .unwrap();
        encoder.encode_into(&mut payload, 3, &source, 75).unwrap();
        encoder.commit(&source);
        assert_eq!(encoder.exact, [false, true]);
        assert!(!encoder.has_stable_exact_debt(now + Duration::from_millis(1999)));
        assert!(encoder.has_stable_exact_debt(now + Duration::from_secs(2)));
        // A real dynamic tile retains priority even when its neighbor is stable.
        source.image.get_pixel_mut(0, 0).0[0] ^= 128;
        let plan = encoder
            .plan_at(
                &source,
                TileEncodeRequest::LosslessRepair,
                now + Duration::from_secs(3),
            )
            .unwrap();
        assert!(plan.emit);
        assert!(!encoder.planned.unwrap().lossless);
    }

    #[test]
    fn lossless_repair_converges_exactly_in_bounded_fair_batches() {
        let source = busy(513, 257);
        let mut encoder = TileEncoder::new();
        let mut decoder = TileDecoder::new();
        encoder.set_lossless_enabled(true);
        decoder.set_lossless_enabled(true);
        let mut payload = Vec::new();
        encode(
            &mut encoder,
            &mut payload,
            1,
            &source,
            100,
            TileEncodeRequest::Keyframe,
        );
        assert_eq!(payload[17] & FLAG_LOSSLESS, 0);
        decoder
            .decode_into(&payload, new_decode_buffer_pool())
            .unwrap();
        encoder.commit(&source);
        assert!(!encoder.has_quality_debt(100));
        assert!(encoder.has_exact_debt());
        let mut sequence = 2;
        while encoder.has_exact_debt() {
            assert!(sequence < 20);
            encode(
                &mut encoder,
                &mut payload,
                sequence,
                &source,
                85,
                TileEncodeRequest::LosslessRepair,
            );
            let timings = encoder.encode_timings().unwrap();
            assert_eq!(timings.jpeg, Duration::ZERO);
            assert!(timings.png.is_some());
            let header = TileHeader::parse(&payload).unwrap();
            assert!(header.lossless && !header.keyframe && header.scroll.is_none());
            let edge = 1usize << header.tile_log2;
            let pixels = usize::from(header.atlas_columns)
                * edge
                * (header.dirty_tiles as usize).div_ceil(usize::from(header.atlas_columns))
                * edge;
            assert!(pixels <= 65_536);
            let decoded = decoder
                .decode_into(&payload, new_decode_buffer_pool())
                .unwrap();
            encoder.commit(&source);
            if !encoder.has_exact_debt() {
                assert_eq!(decoded.image(), &source.image);
            }
            sequence += 1;
        }
        assert!(
            !encoder
                .plan(&source, TileEncodeRequest::LosslessRepair)
                .unwrap()
                .emit
        );
    }

    #[test]
    fn lossless_debt_survives_discard_and_tolerance_changes_and_resets() {
        let mut source = busy(33, 19);
        let mut encoder = TileEncoder::new();
        encoder.set_lossless_enabled(true);
        let mut payload = Vec::new();
        encode(
            &mut encoder,
            &mut payload,
            1,
            &source,
            100,
            TileEncodeRequest::Keyframe,
        );
        encoder.commit(&source);
        encode(
            &mut encoder,
            &mut payload,
            2,
            &source,
            100,
            TileEncodeRequest::LosslessRepair,
        );
        encoder.discard(); // Includes a caller's failed/partial write path.
        assert!(encoder.has_exact_debt());
        encode(
            &mut encoder,
            &mut payload,
            3,
            &source,
            100,
            TileEncodeRequest::LosslessRepair,
        );
        encoder.commit(&source);
        assert!(!encoder.has_exact_debt());
        source.image.get_pixel_mut(0, 0).0[0] ^= 1;
        assert!(
            !encoder
                .plan(&source, TileEncodeRequest::Delta)
                .unwrap()
                .emit
        );
        assert!(encoder.detected_exact_change());
        assert!(encoder.has_exact_debt());
        encode(
            &mut encoder,
            &mut payload,
            4,
            &source,
            85,
            TileEncodeRequest::LosslessRepair,
        );
        assert!(!encoder.detected_exact_change());
        assert_ne!(payload[17] & FLAG_LOSSLESS, 0);
        source.image.get_pixel_mut(0, 0).0[0] ^= 1;
        encoder.commit(&source); // Changed immutable input cannot discharge debt.
        assert!(encoder.has_exact_debt());
        assert!(
            encoder
                .plan(&source, TileEncodeRequest::Delta)
                .unwrap()
                .keyframe
        );
        source.geometry_epoch += 1;
        assert!(
            encoder
                .plan(&source, TileEncodeRequest::LosslessRepair)
                .unwrap()
                .keyframe
        );
    }

    #[test]
    fn lossless_source_geometry_scroll_and_encode_failure_keep_debt() {
        let mut source = busy(96, 160);
        let mut encoder = TileEncoder::new();
        encoder.set_lossless_enabled(true);
        encoder.set_scroll_copy_enabled(true);
        let mut payload = Vec::new();
        encode(
            &mut encoder,
            &mut payload,
            1,
            &source,
            100,
            TileEncodeRequest::Keyframe,
        );
        encoder.commit(&source);
        encode(
            &mut encoder,
            &mut payload,
            2,
            &source,
            100,
            TileEncodeRequest::LosslessRepair,
        );
        encoder.commit(&source);
        assert!(!encoder.has_exact_debt());
        let old = source.image.clone();
        for y in 0..153 {
            for x in 0..96 {
                source.image.put_pixel(x, y, *old.get_pixel(x, y + 7));
            }
        }
        encoder.plan(&source, TileEncodeRequest::Delta).unwrap();
        assert!(encoder.detected.unwrap().scroll.is_some());
        assert!(encoder.has_exact_debt());
        encoder.discard();
        source.geometry_epoch += 1;
        assert!(
            encoder
                .plan(&source, TileEncodeRequest::LosslessRepair)
                .unwrap()
                .keyframe
        );
        encoder.encode_into(&mut payload, 3, &source, 100).unwrap();
        encoder.commit(&source);
        let repair_at =
            encoder.stable_since.iter().copied().max().unwrap() + Duration::from_secs(2);
        assert!(
            encoder
                .plan_at(&source, TileEncodeRequest::LosslessRepair, repair_at)
                .unwrap()
                .emit
        );
        assert!(encoder.planned.unwrap().lossless);
        let epoch = source.geometry_epoch;
        source.geometry_epoch += 1;
        assert!(encoder.encode_into(&mut payload, 4, &source, 100).is_err());
        assert!(payload.is_empty());
        assert!(encoder.has_exact_debt());
        source.geometry_epoch = epoch;
        source.source_width += 1;
        assert!(
            encoder
                .plan(&source, TileEncodeRequest::LosslessRepair)
                .unwrap()
                .keyframe
        );
        assert!(encoder.exact.iter().all(|exact| !exact));
    }

    #[test]
    fn lossless_is_opt_in_and_rejected_records_do_not_mutate_canvas() {
        let source = busy(33, 19);
        let mut encoder = TileEncoder::new();
        let mut decoder = TileDecoder::new();
        let mut payload = Vec::new();
        encode(
            &mut encoder,
            &mut payload,
            1,
            &source,
            85,
            TileEncodeRequest::LosslessRepair,
        );
        assert_eq!(payload[17] & FLAG_LOSSLESS, 0);
        encoder.commit(&source);
        decoder
            .decode_into(&payload, new_decode_buffer_pool())
            .unwrap();
        encoder.set_lossless_enabled(true);
        encoder.plan(&source, TileEncodeRequest::Delta).unwrap();
        encode(
            &mut encoder,
            &mut payload,
            2,
            &source,
            85,
            TileEncodeRequest::LosslessRepair,
        );
        assert!(
            decoder
                .decode_into(&payload, new_decode_buffer_pool())
                .is_err()
        );
        assert_eq!(decoder.last_sequence, Some(1));
        decoder.set_lossless_enabled(true);
        let valid = payload.clone();
        payload[17] |= FLAG_KEYFRAME;
        assert!(
            decoder
                .decode_into(&payload, new_decode_buffer_pool())
                .is_err()
        );
        assert_eq!(decoder.last_sequence, Some(1));
        decoder
            .decode_into(&valid, new_decode_buffer_pool())
            .unwrap();
        assert_eq!(decoder.last_sequence, Some(2));
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
        // Existing codec-only repair tests advance the observation clock
        // explicitly, never sleep or bypass the production two-second gate.
        // Dedicated stability tests below use named absolute timestamps.
        let now = if request == TileEncodeRequest::LosslessRepair {
            encoder
                .stable_since
                .iter()
                .copied()
                .max()
                .unwrap_or_else(Instant::now)
                + Duration::from_secs(2)
        } else {
            Instant::now()
        };
        let plan = encoder.plan_at(frame, request, now).expect("plan succeeds");
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
    fn encode_stage_timings_split_pack_and_jpeg_and_reset_on_keepalive_or_failure() {
        let captured = frame(64, 64, |_, _| Rgb([23, 45, 67]));
        let mut encoder = TileEncoder::new();
        let mut payload = Vec::new();
        encoder
            .plan(&captured, TileEncodeRequest::Keyframe)
            .unwrap();
        let base = Instant::now();
        let mut ticks = [0, 3, 10].into_iter();
        encoder
            .encode_with_clock(&mut payload, 0, &captured, 85, || {
                base + Duration::from_millis(ticks.next().expect("exactly three timing boundaries"))
            })
            .unwrap();
        assert_eq!(ticks.next(), None);
        assert_eq!(
            encoder.encode_timings(),
            Some(EncodeStageTimings {
                atlas_pack: Duration::from_millis(3),
                jpeg: Duration::from_millis(7),
                png: None,
            })
        );
        encoder.commit(&captured);
        encoder
            .plan(&captured, TileEncodeRequest::Keepalive)
            .unwrap();
        encoder
            .encode_with_clock(&mut payload, 1, &captured, 85, || {
                panic!("keepalive must not run atlas stages")
            })
            .unwrap();
        assert_eq!(encoder.encode_timings(), None);
        encoder
            .plan(&captured, TileEncodeRequest::Keyframe)
            .unwrap();
        encoder.encode_into(&mut payload, 2, &captured, 85).unwrap();
        assert!(encoder.encode_timings().is_some());
        assert!(encoder.encode_into(&mut payload, 3, &captured, 85).is_err());
        assert_eq!(encoder.encode_timings(), None);
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
        assert_eq!(
            decoded.damage(),
            Some(FrameDamage {
                base_sequence: 7,
                regions: DamageRegions::from_rect(None)
            })
        );
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
    fn same_size_geometry_epoch_forces_keyframe_and_rejects_old_deltas() {
        let mut encoder = TileEncoder::new();
        let mut decoder = TileDecoder::new();
        let pool = new_decode_buffer_pool();
        let mut first = flat(64, 64, 10);
        first.geometry_epoch = 1;
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
        let decoded = decoder.decode_into(&payload, pool.clone()).unwrap();
        assert_eq!(decoded.geometry_epoch(), 1);
        let baseline = payload.clone();
        first.geometry_epoch = 2;
        let plan = encode(
            &mut encoder,
            &mut payload,
            1,
            &first,
            90,
            TileEncodeRequest::Delta,
        );
        assert!(plan.keyframe && plan.emit);
        encoder.commit(&first);
        let decoded = decoder.decode_into(&payload, pool.clone()).unwrap();
        assert_eq!(decoded.geometry_epoch(), 2);
        assert!(decoded.damage().is_none());
        assert!(decoder.decode_into(&baseline, pool.clone()).is_err());
        encode(
            &mut encoder,
            &mut payload,
            2,
            &first,
            90,
            TileEncodeRequest::Keepalive,
        );
        payload[24..32].copy_from_slice(&3_u64.to_be_bytes());
        assert!(decoder.decode_into(&payload, pool).is_err());
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
    fn source_geometry_delta_is_rejected_without_changing_decoder_state() {
        for source_offset in [8, 10] {
            for dirty in [false, true] {
                let mut encoder = TileEncoder::new();
                let mut decoder = TileDecoder::new();
                let first = flat(64, 64, 10);
                roundtrip(
                    &mut encoder,
                    &mut decoder,
                    &first,
                    90,
                    TileEncodeRequest::Keyframe,
                );
                let original_source = decoder.source;
                let original_canvas = decoder.canvas.clone();
                let original_atlas = decoder.atlas.clone();
                let mut next = flat(64, 64, 10);
                if dirty {
                    next.image.put_pixel(1, 1, Rgb([255; 3]));
                }
                let mut payload = Vec::new();
                let plan = encode(
                    &mut encoder,
                    &mut payload,
                    8,
                    &next,
                    90,
                    TileEncodeRequest::Keepalive,
                );
                assert!(!plan.keyframe);
                assert_eq!(plan.dirty_tiles > 0, dirty);
                let valid_payload = payload.clone();
                payload[source_offset..source_offset + 2].copy_from_slice(&256_u16.to_be_bytes());
                let result = decoder.decode_into(&payload, new_decode_buffer_pool());
                assert!(result.is_err(), "a delta cannot change source geometry");
                assert_eq!(decoder.source, original_source);
                assert_eq!(decoder.canvas, original_canvas);
                assert_eq!(decoder.atlas, original_atlas);
                let recovered = decoder
                    .decode_into(&valid_payload, new_decode_buffer_pool())
                    .unwrap();
                assert_eq!(recovered.source_width(), first.source_width);
                assert_eq!(recovered.source_height(), first.source_height);
            }
        }
    }

    #[test]
    fn source_geometry_keyframe_establishes_the_next_delta_coordinate_space() {
        let mut encoder = TileEncoder::new();
        let mut decoder = TileDecoder::new();
        let mut source = flat(64, 64, 10);
        for (width, height) in [(128, 128), (256, 128), (256, 256)] {
            source.source_width = width;
            source.source_height = height;
            let (plan, _, _) = roundtrip(
                &mut encoder,
                &mut decoder,
                &source,
                90,
                TileEncodeRequest::Delta,
            );
            assert!(plan.keyframe);
            assert_eq!(decoder.source, (width, height));
            let (keepalive, _, _) = roundtrip(
                &mut encoder,
                &mut decoder,
                &source,
                90,
                TileEncodeRequest::Keepalive,
            );
            assert!(!keepalive.keyframe);
            assert_eq!(keepalive.dirty_tiles, 0);
            assert_eq!(decoder.source, (width, height));
        }
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
    #[test]
    fn refinement_selection_is_bounded_fair_and_never_downgrades() {
        let mut qualities = vec![40; 10];
        let mut cursor = 0;
        let mut visited = [false; 10];
        for target in [60, 61, 62, 63] {
            let (indices, next) = refinement_tiles(&qualities, target, 3, cursor);
            assert!(indices.len() <= 3);
            assert!(indices.windows(2).all(|pair| pair[0] < pair[1]));
            for index in indices {
                assert!(qualities[index] < target);
                qualities[index] = target;
                visited[index] = true;
            }
            cursor = next;
        }
        assert!(
            visited.iter().all(|seen| *seen),
            "rising quality must not starve the tail"
        );
        assert!(refinement_tiles(&qualities, 40, 3, cursor).0.is_empty());
        assert!(refinement_tiles(&qualities, 85, 0, cursor).0.is_empty());
        assert_eq!(refinement_tiles(&[], 85, 3, usize::MAX), (vec![], 0));
        let (wrapped, _) = refinement_tiles(&[40; 5], 85, 3, 4);
        assert_eq!(
            wrapped,
            vec![0, 1, 4],
            "atlas indices must remain raster sorted"
        );
    }

    #[test]
    fn refinement_codec_converges_then_suppresses_without_a_keyframe() {
        let source = busy(320, 240);
        let mut encoder = TileEncoder::new();
        let mut decoder = TileDecoder::new();
        let (_, _, low) = roundtrip(
            &mut encoder,
            &mut decoder,
            &source,
            40,
            TileEncodeRequest::Keyframe,
        );
        let low = low.unwrap();
        let mut payload = Vec::new();
        let mut refined = low.clone();
        let mut rounds = 0;
        while encoder.has_quality_debt(85) {
            let plan = encoder
                .plan_with_refinement(&source, TileEncodeRequest::Delta, Some((85, 256)))
                .unwrap();
            assert!(!plan.keyframe && plan.emit && plan.dirty_tiles <= 256);
            encoder
                .encode_into(&mut payload, rounds, &source, 85)
                .unwrap();
            assert!(
                payload.len() < 512 * 1024,
                "bounded production JPEG fixture payload"
            );
            eprintln!(
                "refinement codec: round={rounds} tiles={} bytes={}",
                plan.dirty_tiles,
                payload.len()
            );
            refined = decoder
                .decode_into(&payload, new_decode_buffer_pool())
                .unwrap()
                .image()
                .clone();
            encoder.commit(&source);
            rounds += 1;
            assert!(rounds <= 2);
        }
        assert_eq!(rounds, 2);
        let error = |image: &RgbImage| -> u64 {
            source
                .image
                .as_raw()
                .iter()
                .zip(image.as_raw())
                .map(|(a, b)| u64::from(a.abs_diff(*b)))
                .sum()
        };
        assert!(
            error(&refined) < error(&low),
            "real JPEG refinement should reduce fixture error"
        );
        assert!(
            !encoder
                .plan_with_refinement(&source, TileEncodeRequest::Delta, Some((85, 256)))
                .unwrap()
                .emit
        );
        let keepalive = encoder
            .plan_with_refinement(&source, TileEncodeRequest::Keepalive, Some((85, 256)))
            .unwrap();
        assert!(keepalive.emit && keepalive.dirty_tiles == 0);
    }

    #[test]
    fn refinement_discard_and_superseded_plan_never_pay_quality_debt() {
        let source = busy(64, 64);
        let mut encoder = TileEncoder::new();
        let mut decoder = TileDecoder::new();
        roundtrip(
            &mut encoder,
            &mut decoder,
            &source,
            40,
            TileEncodeRequest::Keyframe,
        );
        let mut payload = Vec::new();
        encoder
            .plan_with_refinement(&source, TileEncodeRequest::Delta, Some((85, 3)))
            .unwrap();
        encoder.encode_into(&mut payload, 1, &source, 85).unwrap();
        assert!(encoder.qualities.iter().all(|q| *q == 40));
        encoder.discard();
        encoder.commit(&source);
        assert!(encoder.qualities.iter().all(|q| *q == 40));
        assert_eq!(encoder.refinement_cursor, 0);
        encoder
            .plan_with_refinement(&source, TileEncodeRequest::Delta, Some((85, 3)))
            .unwrap();
        encoder.encode_into(&mut payload, 2, &source, 85).unwrap();
        encoder.plan(&source, TileEncodeRequest::Delta).unwrap();
        encoder.commit(&source);
        assert!(
            encoder.qualities.iter().all(|q| *q == 40),
            "a new plan invalidates an older uncommitted encode"
        );
        assert_eq!(encoder.refinement_cursor, 0);
    }

    #[test]
    fn refinement_reselection_reuses_damage_and_drops_stale_repairs() {
        let source = flat(64, 64, 20);
        let mut encoder = TileEncoder::new();
        let mut decoder = TileDecoder::new();
        roundtrip(
            &mut encoder,
            &mut decoder,
            &source,
            70,
            TileEncodeRequest::Keyframe,
        );
        let mut changed = flat(64, 64, 20);
        changed.image.put_pixel(63, 63, Rgb([250; 3]));
        assert_eq!(
            encoder
                .plan(&changed, TileEncodeRequest::Delta)
                .unwrap()
                .dirty_tiles,
            1
        );
        assert_eq!(
            encoder
                .select_refinement(Some((85, 3)))
                .unwrap()
                .dirty_tiles,
            4
        );
        assert_eq!(
            encoder
                .select_refinement(Some((60, 3)))
                .unwrap()
                .dirty_tiles,
            1
        );
        assert_eq!(encoder.dirty, vec![15]);
        assert_eq!(encoder.bitmap, vec![0, 128]);
        assert_eq!(encoder.detection_scans, 1);
        let mut payload = Vec::new();
        encoder.encode_into(&mut payload, 1, &changed, 60).unwrap();
        assert!(
            encoder.select_refinement(Some((85, 3))).is_err(),
            "an encoded detection cannot be reused for another capture"
        );
        encoder.discard();
        encoder.commit(&changed);
        assert_eq!(encoder.reference(), Some(&source.image));
        assert!(encoder.qualities.iter().all(|q| *q == 70));
        assert_eq!(encoder.refinement_cursor, 0);
        assert_eq!(
            encoder
                .plan(&changed, TileEncodeRequest::Delta)
                .unwrap()
                .dirty_tiles,
            1
        );
        assert_eq!(encoder.detection_scans, 2);
    }

    #[test]
    fn blinking_tile_does_not_starve_unchanged_text_or_reorder_the_atlas() {
        let mut source = flat(128, 32, 20);
        let mut encoder = TileEncoder::new();
        let mut decoder = TileDecoder::new();
        roundtrip(
            &mut encoder,
            &mut decoder,
            &source,
            40,
            TileEncodeRequest::Keyframe,
        );
        // Start in the middle: the changed high-index tile is collected first,
        // while refinement wraps around to low indices on later rounds.
        encoder.refinement_cursor = 8;
        let mut payload = Vec::new();
        for sequence in 1..=16 {
            let blink = if sequence % 2 == 0 { 20 } else { 240 };
            for y in 16..32 {
                for x in 112..128 {
                    source.image.put_pixel(x, y, Rgb([blink; 3]));
                }
            }
            let plan = encoder
                .plan_with_refinement(&source, TileEncodeRequest::Delta, Some((85, 2)))
                .unwrap();
            assert!(plan.dirty_tiles <= 3, "damage plus bounded repair");
            encoder
                .encode_into(&mut payload, sequence, &source, 85)
                .unwrap();
            decoder
                .decode_into(&payload, new_decode_buffer_pool())
                .unwrap();
            encoder.commit(&source);
            let decoded = decoder.canvas().unwrap();
            assert!(decoded.get_pixel(120, 24)[0].abs_diff(blink) < 12);
            assert!(decoded.get_pixel(8, 8)[0].abs_diff(20) < 12);
        }
        assert!(!encoder.has_quality_debt(85));
    }

    #[test]
    fn refinement_damage_and_geometry_changes_replace_quality_state() {
        let source = flat(64, 64, 20);
        let mut encoder = TileEncoder::new();
        let mut decoder = TileDecoder::new();
        roundtrip(
            &mut encoder,
            &mut decoder,
            &source,
            40,
            TileEncodeRequest::Keyframe,
        );
        let mut changed = flat(64, 64, 20);
        changed.image.put_pixel(0, 0, Rgb([250; 3]));
        let plan = encoder
            .plan_with_refinement(&changed, TileEncodeRequest::Delta, Some((85, 1)))
            .unwrap();
        assert_eq!(
            plan.dirty_tiles, 1,
            "a changed tile also pays its own quality debt"
        );
        let mut payload = Vec::new();
        encoder.encode_into(&mut payload, 1, &changed, 70).unwrap();
        encoder.commit(&changed);
        assert_eq!(encoder.qualities[0], 70);
        assert!(encoder.qualities[1..].iter().all(|q| *q == 40));
        let plan = encoder
            .plan_with_refinement(&changed, TileEncodeRequest::Delta, Some((50, 256)))
            .unwrap();
        assert_eq!(
            plan.dirty_tiles, 15,
            "quality decreases cannot downgrade the sharper tile"
        );
        encoder.encode_into(&mut payload, 2, &changed, 50).unwrap();
        encoder.commit(&changed);
        assert_eq!(encoder.qualities[0], 70);
        assert!(!encoder.has_quality_debt(50));
        let resized = flat(16, 16, 20);
        roundtrip(
            &mut encoder,
            &mut decoder,
            &resized,
            60,
            TileEncodeRequest::Delta,
        );
        assert_eq!(encoder.qualities, vec![60]);
        assert_eq!(encoder.refinement_cursor, 0);
        let mut new_source = resized;
        new_source.source_width += 1;
        roundtrip(
            &mut encoder,
            &mut decoder,
            &new_source,
            45,
            TileEncodeRequest::Delta,
        );
        assert_eq!(encoder.qualities, vec![45]);
        assert!(TileEncoder::new().qualities.is_empty());
    }
    #[test]
    fn refinement_selection_trace_converges_and_retargets_without_downgrading() {
        let mut qualities = vec![40; 600];
        let (first, next) = refinement_tiles(&qualities, 70, 256, 0);
        assert_eq!(first.len(), 256);
        // Plan/discard leaves the committed ledger and cursor unchanged.
        assert_eq!(refinement_tiles(&qualities, 70, 256, 0).0, first);
        for index in first {
            qualities[index] = 70;
        }
        let mut cursor = next;
        let mut batches = 0;
        loop {
            let (indices, next) = refinement_tiles(&qualities, 50, 256, cursor);
            if indices.is_empty() {
                break;
            }
            assert!(indices.iter().all(|index| *index >= 256));
            for index in indices {
                qualities[index] = 50;
            }
            cursor = next;
            batches += 1;
        }
        assert_eq!(batches, 2);
        assert!(qualities[..256].iter().all(|q| *q == 70));
        assert!(qualities[256..].iter().all(|q| *q == 50));
        assert!(refinement_tiles(&qualities, 50, 256, cursor).0.is_empty());
        let (indices, _) = refinement_tiles(&qualities, 85, usize::MAX, usize::MAX);
        assert_eq!(indices.len(), qualities.len());
    }
}

#[cfg(test)]
#[path = "tile_scroll_tests.rs"]
mod scroll_tests;

#[cfg(test)]
#[path = "tile_scroll_benchmark.rs"]
mod scroll_benchmark;

#[cfg(test)]
mod atlas_envelope_tests {
    use super::*;
    #[test]
    fn complete_production_jpeg_rejects_truncation_and_trailing_payload() {
        let mut jpeg = Vec::new();
        JpegEncoder::new_with_quality(&mut jpeg, 75)
            .encode(&[127; 16 * 16 * 3], 16, 16, ExtendedColorType::Rgb8)
            .unwrap();
        assert!(validate_atlas_jpeg_envelope(&jpeg).is_ok());
        for cut in [0, 1, jpeg.len() / 2, jpeg.len() - 2, jpeg.len() - 1] {
            assert!(validate_atlas_jpeg_envelope(&jpeg[..cut]).is_err());
        }
        for suffix in [vec![0], vec![0xff, 0xd9], jpeg.clone()] {
            let mut trailing = jpeg.clone();
            trailing.extend(suffix);
            assert!(validate_atlas_jpeg_envelope(&trailing).is_err());
        }
        let mut atlas = Vec::new();
        decode_atlas(&mut atlas, &jpeg, 16, 16, jpeg.len()).unwrap();
        assert_eq!(atlas.len(), 16 * 16 * 3);
    }
}

#[cfg(test)]
#[path = "tile_lossless_baseline.rs"]
mod lossless_baseline;

#[cfg(test)]
#[path = "tile_stability_benchmark.rs"]
mod stability_benchmark;

#[cfg(test)]
#[path = "tile_scroll_exact_tests.rs"]
mod scroll_exact_tests;
