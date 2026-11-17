//! Bounded destination damage and nearest-neighbor SHM patching, without any
//! Wayland objects. Presentation and buffer ownership remain in the caller.
use super::super::frame::DamageRegions;
use super::MAX_UPLOAD_SCRATCH_BYTES;
use std::io::{self, Seek, SeekFrom, Write};

/// None means the slot has no reusable baseline. Empty regions mean it already
/// contains the complete current image. Busy slots accumulate without writing.
#[derive(Default)]
pub(super) struct SlotDamage {
    dirty: Option<DamageRegions>,
}

impl SlotDamage {
    pub(super) fn accumulate(&mut self, delta: Option<DamageRegions>, size: (u16, u16)) {
        match (&mut self.dirty, delta) {
            (Some(dirty), Some(delta)) => {
                for &rect in delta.as_slice() {
                    dirty.add(rect, size);
                }
            }
            _ => self.dirty = None,
        }
    }

    pub(super) fn is_full(&self) -> bool {
        self.dirty.is_none()
    }

    pub(super) fn regions(&self, size: (u16, u16)) -> DamageRegions {
        self.dirty.unwrap_or_else(|| {
            let mut full = DamageRegions::default();
            full.add((0, 0, size.0, size.1), size);
            full
        })
    }

    // Only call after every write succeeded, never when a buffer is released.
    pub(super) fn uploaded(&mut self) {
        self.dirty = Some(DamageRegions::default());
    }
}

#[derive(Clone, Copy)]
struct Layout {
    source: (u16, u16),
    destination: (u16, u16),
    content: (u16, u16),
    offset: (u16, u16),
}

impl Layout {
    fn new(source: (u16, u16), destination: (u16, u16)) -> Option<Self> {
        let (iw, ih) = (u64::from(source.0), u64::from(source.1));
        let (width, height) = (u64::from(destination.0), u64::from(destination.1));
        if iw == 0 || ih == 0 || width == 0 || height == 0 {
            return None;
        }
        let content = if width * ih <= height * iw {
            (destination.0, (width * ih / iw).max(1) as u16)
        } else {
            ((height * iw / ih).max(1) as u16, destination.1)
        };
        Some(Self {
            source,
            destination,
            content,
            offset: (
                (destination.0 - content.0) / 2,
                (destination.1 - content.1) / 2,
            ),
        })
    }
}

fn valid_rect((x, y, w, h): (u16, u16, u16, u16), size: (u16, u16)) -> bool {
    w > 0
        && h > 0
        && u32::from(x) + u32::from(w) <= u32::from(size.0)
        && u32::from(y) + u32::from(h) <= u32::from(size.1)
}

/// Invert floor(destination * source / content) exactly: both edges use ceil.
/// A source pixel skipped by downscaling produces an empty destination range.
/// Invalid metadata falls back to a complete upload, never a clipped guess.
pub(super) fn map_regions(
    source: (u16, u16),
    destination: (u16, u16),
    damage: &DamageRegions,
) -> Option<DamageRegions> {
    let layout = Layout::new(source, destination)?;
    let edge = |value: u16, target: u16, original: u16| {
        (u64::from(value) * u64::from(target)).div_ceil(u64::from(original)) as u16
    };
    let mut mapped = DamageRegions::default();
    for &(x, y, w, h) in damage.as_slice() {
        if !valid_rect((x, y, w, h), source) {
            return None;
        }
        let left = edge(x, layout.content.0, source.0);
        let right = edge(x + w, layout.content.0, source.0);
        let top = edge(y, layout.content.1, source.1);
        let bottom = edge(y + h, layout.content.1, source.1);
        mapped.add(
            (
                layout.offset.0 + left,
                layout.offset.1 + top,
                right - left,
                bottom - top,
            ),
            destination,
        );
    }
    Some(mapped)
}

/// Write only destination spans, using the same native XRGB packing and black
/// letterbox policy as a full upload. Full-width spans still batch whole rows.
/// All offsets are absolute; short/interrupted writes retain write_all's rules.
pub(super) fn upload_regions(
    writer: &mut (impl Write + Seek),
    rgb: &[u8],
    source: (u16, u16),
    destination: (u16, u16),
    damage: &DamageRegions,
    scratch: &mut [u8],
) -> io::Result<()> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidInput, "invalid incremental upload");
    let layout = Layout::new(source, destination).ok_or_else(invalid)?;
    let source_bytes = usize::from(source.0)
        .checked_mul(usize::from(source.1))
        .and_then(|pixels| pixels.checked_mul(3));
    if source_bytes != Some(rgb.len())
        || scratch.len() < usize::from(destination.0) * 4
        || scratch.len() > MAX_UPLOAD_SCRATCH_BYTES
        || damage
            .as_slice()
            .iter()
            .any(|&rect| !valid_rect(rect, destination))
    {
        return Err(invalid());
    }
    for &(x, y, w, h) in damage.as_slice() {
        let row_bytes = usize::from(w) * 4;
        // Partial-width rows have gaps in the memfd, so write each span at its
        // exact offset rather than overwriting unchanged neighboring pixels.
        let rows_per_chunk = if w == layout.destination.0 {
            scratch.len() / row_bytes
        } else {
            1
        };
        for first_row in (0..usize::from(h)).step_by(rows_per_chunk) {
            let rows = rows_per_chunk.min(usize::from(h) - first_row);
            let chunk = &mut scratch[..rows * row_bytes];
            chunk.fill(0);
            for row in 0..rows {
                let dy = usize::from(y) + first_row + row;
                if dy < usize::from(layout.offset.1)
                    || dy >= usize::from(layout.offset.1 + layout.content.1)
                {
                    continue;
                }
                let sy = (dy - usize::from(layout.offset.1)) * usize::from(layout.source.1)
                    / usize::from(layout.content.1);
                let left = x.max(layout.offset.0);
                let right = (x + w).min(layout.offset.0 + layout.content.0);
                for dx in left..right {
                    let sx = usize::from(dx - layout.offset.0) * usize::from(layout.source.0)
                        / usize::from(layout.content.0);
                    let at = (sy * usize::from(source.0) + sx) * 3;
                    let pixel = (u32::from(rgb[at]) << 16)
                        | (u32::from(rgb[at + 1]) << 8)
                        | u32::from(rgb[at + 2]);
                    let output = row * row_bytes + usize::from(dx - x) * 4;
                    chunk[output..output + 4].copy_from_slice(&pixel.to_ne_bytes());
                }
            }
            let offset =
                ((u64::from(y) + first_row as u64) * u64::from(destination.0) + u64::from(x)) * 4;
            writer.seek(SeekFrom::Start(offset))?;
            writer.write_all(chunk)?;
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "wayland_viewer_damage_tests.rs"]
mod tests;
