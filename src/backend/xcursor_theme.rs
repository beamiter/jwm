// src/backend/xcursor_theme.rs
//
// Shared Xcursor theme handling. Backends that render or install a real pointer
// (the Wayland DRM/KMS software cursor and the X11RB/XCB RENDER cursors) load
// their images through this module so a single `[appearance]` cursor_theme /
// cursor_size configuration drives every backend consistently — the macOS-style
// "one pointer for the whole session" behavior.

use std::collections::HashMap;
use std::io::Read as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;

use xcursor::{
    CursorTheme,
    parser::{Image, parse_xcursor},
};

use crate::backend::common_define::StdCursorKind;

/// Cursor files are raw pixel containers. A generous ceiling still covers
/// large animated themes while bounding both the source buffer and the two
/// pixel copies produced by the third-party parser.
const MAX_XCURSOR_FILE_BYTES: u64 = 16 * 1024 * 1024;
/// `xcursor` keeps both the source-order and converted pixels for every TOC
/// entry. Count repeated offsets too so a tiny table cannot make it decode the
/// same large image hundreds of times.
const MAX_XCURSOR_DECODED_RGBA_BYTES: usize = 32 * 1024 * 1024;
/// Real cursor files have at most a handful of sizes/animation frames. Capping
/// the table also prevents `parse_xcursor` from reserving an attacker-chosen
/// `u32` number of `Image` slots after it has scanned the table.
const MAX_XCURSOR_TOC_ENTRIES: u32 = 1024;
const XCURSOR_HEADER_BYTES: usize = 16;
const XCURSOR_TOC_ENTRY_BYTES: usize = 12;
const XCURSOR_IMAGE_HEADER_BYTES: usize = 36;
const XCURSOR_IMAGE_TYPE: u32 = 0xfffd_0002;

/// Freedesktop cursor names to try, in priority order, for each logical kind.
/// The final `"default"` entry guarantees a fallback for minimal themes.
pub fn cursor_candidates(kind: StdCursorKind) -> &'static [&'static str] {
    match kind {
        StdCursorKind::LeftPtr => &["left_ptr", "default"],
        StdCursorKind::Hand => &["hand2", "hand1", "pointer", "default"],
        StdCursorKind::XTerm => &["xterm", "text", "default"],
        StdCursorKind::Watch => &["watch", "wait", "default"],
        StdCursorKind::Crosshair => &["crosshair", "default"],
        StdCursorKind::Fleur => &["fleur", "move", "default"],
        StdCursorKind::HDoubleArrow => &["sb_h_double_arrow", "h_double_arrow", "default"],
        StdCursorKind::VDoubleArrow => &["sb_v_double_arrow", "v_double_arrow", "default"],
        StdCursorKind::TopLeftCorner => &["top_left_corner", "nw-resize", "default"],
        StdCursorKind::TopRightCorner => &["top_right_corner", "ne-resize", "default"],
        StdCursorKind::BottomLeftCorner => &["bottom_left_corner", "sw-resize", "default"],
        StdCursorKind::BottomRightCorner => &["bottom_right_corner", "se-resize", "default"],
        StdCursorKind::Sizing => &["sizing", "default"],
    }
}

/// Pick the frame whose nominal size is closest to `target_size`. We don't
/// animate, so for animated cursors we return the first frame of that size.
pub fn pick_nearest_image(images: &[Image], target_size: u32) -> Option<&Image> {
    let nearest = images
        .iter()
        .min_by_key(|img| target_size.abs_diff(img.size))?;
    images
        .iter()
        .find(|img| img.width == nearest.width && img.height == nearest.height)
}

fn read_xcursor_file(path: &Path) -> Option<Vec<u8>> {
    // Follow theme symlinks, but reject devices/FIFOs before opening so a
    // broken theme cannot block compositor startup waiting for a writer.
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_XCURSOR_FILE_BYTES {
        return None;
    }

    // O_NONBLOCK closes the metadata/open race for a path replaced with a
    // FIFO; it has no effect on reads from an ordinary file.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .ok()?;
    let opened_metadata = file.metadata().ok()?;
    if !opened_metadata.is_file() || opened_metadata.len() > MAX_XCURSOR_FILE_BYTES {
        return None;
    }
    let mut data = Vec::with_capacity(opened_metadata.len() as usize);
    file.take(MAX_XCURSOR_FILE_BYTES + 1)
        .read_to_end(&mut data)
        .ok()?;
    (data.len() as u64 <= MAX_XCURSOR_FILE_BYTES).then_some(data)
}

fn u32_at(data: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        data.get(offset..offset.checked_add(4)?)?.try_into().ok()?,
    ))
}

fn decoded_rgba_bytes(data: &[u8]) -> Option<usize> {
    if data.len() < XCURSOR_HEADER_BYTES || data.get(..4)? != b"Xcur" {
        return None;
    }
    let header_len = usize::try_from(u32_at(data, 4)?).ok()?;
    let toc_entries = u32_at(data, 12)?;
    if header_len < XCURSOR_HEADER_BYTES || toc_entries > MAX_XCURSOR_TOC_ENTRIES {
        return None;
    }
    let toc_bytes = usize::try_from(toc_entries)
        .ok()?
        .checked_mul(XCURSOR_TOC_ENTRY_BYTES)?;
    if header_len.checked_add(toc_bytes)? > data.len() {
        return None;
    }

    let mut decoded_bytes = 0_usize;
    for index in 0..usize::try_from(toc_entries).ok()? {
        let toc_offset = header_len.checked_add(index.checked_mul(XCURSOR_TOC_ENTRY_BYTES)?)?;
        if u32_at(data, toc_offset)? != XCURSOR_IMAGE_TYPE {
            continue;
        }
        let image_offset = usize::try_from(u32_at(data, toc_offset.checked_add(8)?)?).ok()?;
        let width = usize::try_from(u32_at(data, image_offset.checked_add(16)?)?).ok()?;
        let height = usize::try_from(u32_at(data, image_offset.checked_add(20)?)?).ok()?;
        let pixels = width.checked_mul(height)?.checked_mul(4)?;
        let image_end = image_offset
            .checked_add(XCURSOR_IMAGE_HEADER_BYTES)?
            .checked_add(pixels)?;
        if image_end > data.len() {
            return None;
        }
        decoded_bytes = decoded_bytes.checked_add(pixels)?;
    }
    Some(decoded_bytes)
}

fn parse_bounded_xcursor_with_limit(data: &[u8], decoded_limit: usize) -> Option<Vec<Image>> {
    if decoded_rgba_bytes(data)? > decoded_limit {
        return None;
    }
    parse_xcursor(data)
}

fn parse_bounded_xcursor(data: &[u8]) -> Option<Vec<Image>> {
    parse_bounded_xcursor_with_limit(data, MAX_XCURSOR_DECODED_RGBA_BYTES)
}

/// A cursor image resolved from the theme, in a backend-neutral form.
///
/// `pixels_argb_le` holds premultiplied pixels packed as little-endian ARGB —
/// i.e. byte order `[B, G, R, A]` per pixel. This matches both DRM
/// `Fourcc::Argb8888` and (on a little-endian X server) an XRENDER
/// `a8r8g8b8` picture uploaded via `PutImage`.
#[derive(Clone)]
pub struct ResolvedCursor {
    pub width: u32,
    pub height: u32,
    pub xhot: u32,
    pub yhot: u32,
    pub pixels_argb_le: Vec<u8>,
}

/// Loads and caches themed cursor images according to the live configuration.
pub struct XcursorImages {
    theme: CursorTheme,
    theme_name: String,
    size: u32,
    /// Parsed images per freedesktop cursor name (all sizes the file provides).
    images: HashMap<String, Vec<Image>>,
}

impl XcursorImages {
    /// Build a loader from the resolved `[appearance]` cursor configuration.
    pub fn from_config() -> Self {
        let (theme_name, size) = crate::config::CONFIG.load().resolved_cursor();
        let mut images = Self {
            theme: CursorTheme::load(&theme_name),
            theme_name,
            size,
            images: HashMap::new(),
        };
        images.probe_availability();
        images
    }

    /// Try to resolve the base pointer at 1× and log the result, so a
    /// misconfigured or missing theme (e.g. `cursor_theme` naming a theme that
    /// is not installed) is diagnosable instead of silently degrading to the
    /// backend's built-in glyph cursor. A `None` here means every kind will
    /// fall back and the configured `cursor_size` is effectively ignored.
    fn probe_availability(&mut self) {
        if self.resolve(StdCursorKind::LeftPtr, 1).is_some() {
            log::info!(
                "[cursor] theme {:?} resolved (size={}px)",
                self.theme_name,
                self.size
            );
        } else {
            log::warn!(
                "[cursor] theme {:?} provides no usable pointer image \
                 (not installed, or missing a left_ptr/default cursor); \
                 falling back to built-in glyph cursors and ignoring \
                 cursor_size={}px",
                self.theme_name,
                self.size
            );
        }
    }

    pub fn size(&self) -> u32 {
        self.size
    }

    pub fn theme_name(&self) -> &str {
        &self.theme_name
    }

    /// Re-read the theme/size from the live config. Reloads the theme only when
    /// its name changed. Returns `true` when either the theme or size changed,
    /// so callers know to drop any cursors they built from the old settings.
    pub fn reload_from_config(&mut self) -> bool {
        let (theme_name, size) = crate::config::CONFIG.load().resolved_cursor();
        let mut changed = false;
        if theme_name != self.theme_name {
            self.theme = CursorTheme::load(&theme_name);
            self.theme_name = theme_name;
            self.images.clear();
            changed = true;
        }
        if size != self.size {
            self.size = size;
            changed = true;
        }
        if changed {
            self.probe_availability();
        }
        changed
    }

    fn load_images(&mut self, name: &str) -> &Vec<Image> {
        if !self.images.contains_key(name) {
            let images = self
                .theme
                .load_icon(name)
                .and_then(|path| {
                    let data = read_xcursor_file(&path)?;
                    parse_bounded_xcursor(&data)
                })
                .unwrap_or_default();
            self.images.insert(name.to_string(), images);
        }
        self.images.get(name).expect("just inserted")
    }

    /// Resolve the best image for `kind` at the given integer `scale`
    /// (physical size = configured size × scale). Returns `None` when the theme
    /// provides no usable image for any candidate name — the caller should then
    /// fall back to whatever built-in cursor it has.
    pub fn resolve(&mut self, kind: StdCursorKind, scale: u32) -> Option<ResolvedCursor> {
        let target_size = self.size.saturating_mul(scale.max(1));
        for &name in cursor_candidates(kind) {
            let images = self.load_images(name);
            if images.is_empty() {
                continue;
            }
            let Some(img) = pick_nearest_image(images, target_size) else {
                continue;
            };
            if img.pixels_rgba.is_empty() || img.width == 0 || img.height == 0 {
                continue;
            }
            return Some(ResolvedCursor {
                width: img.width,
                height: img.height,
                xhot: img.xhot,
                yhot: img.yhot,
                // `pixels_rgba` is the raw little-endian ARGB payload from the
                // Xcursor file, i.e. byte order [B, G, R, A]. See the type doc.
                pixels_argb_le: img.pixels_rgba.clone(),
            });
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST_PATH: AtomicU64 = AtomicU64::new(0);

    fn test_path(name: &str) -> std::path::PathBuf {
        let sequence = NEXT_TEST_PATH.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "jwm-xcursor-{name}-{}-{sequence}",
            std::process::id()
        ))
    }

    fn image(size: u32, width: u32) -> Image {
        Image {
            size,
            width,
            height: 1,
            xhot: 0,
            yhot: 0,
            delay: 0,
            pixels_rgba: vec![0; width as usize * 4],
            pixels_argb: vec![0; width as usize * 4],
        }
    }

    fn header(toc_entries: u32) -> Vec<u8> {
        let mut data = b"Xcur".to_vec();
        data.extend_from_slice(&(XCURSOR_HEADER_BYTES as u32).to_le_bytes());
        data.extend_from_slice(&1_u32.to_le_bytes());
        data.extend_from_slice(&toc_entries.to_le_bytes());
        data
    }

    fn repeated_image(entries: u32, width: u32, height: u32) -> Vec<u8> {
        let image_offset = XCURSOR_HEADER_BYTES + entries as usize * XCURSOR_TOC_ENTRY_BYTES;
        let mut data = header(entries);
        for _ in 0..entries {
            data.extend_from_slice(&XCURSOR_IMAGE_TYPE.to_le_bytes());
            data.extend_from_slice(&width.to_le_bytes());
            data.extend_from_slice(&(image_offset as u32).to_le_bytes());
        }
        data.extend_from_slice(&(XCURSOR_IMAGE_HEADER_BYTES as u32).to_le_bytes());
        data.extend_from_slice(&XCURSOR_IMAGE_TYPE.to_le_bytes());
        data.extend_from_slice(&width.to_le_bytes());
        data.extend_from_slice(&1_u32.to_le_bytes());
        data.extend_from_slice(&width.to_le_bytes());
        data.extend_from_slice(&height.to_le_bytes());
        data.extend_from_slice(&0_u32.to_le_bytes());
        data.extend_from_slice(&0_u32.to_le_bytes());
        data.extend_from_slice(&0_u32.to_le_bytes());
        data.resize(
            image_offset + XCURSOR_IMAGE_HEADER_BYTES + width as usize * height as usize * 4,
            0,
        );
        data
    }

    #[test]
    fn nearest_image_handles_untrusted_u32_sizes_without_overflow() {
        let images = [image(0x8000_0001, 8), image(2, 2)];
        assert_eq!(pick_nearest_image(&images, 1).unwrap().width, 2);
    }

    #[test]
    fn bounded_parser_rejects_excessive_toc_reservations() {
        let entries = MAX_XCURSOR_TOC_ENTRIES + 1;
        let mut data = header(entries);
        data.resize(
            XCURSOR_HEADER_BYTES + entries as usize * XCURSOR_TOC_ENTRY_BYTES,
            0,
        );
        assert!(parse_bounded_xcursor(&data).is_none());

        assert_eq!(parse_bounded_xcursor(&header(0)), Some(Vec::new()));
    }

    #[test]
    fn bounded_parser_counts_repeated_image_offsets() {
        let data = repeated_image(3, 2, 2);
        assert_eq!(decoded_rgba_bytes(&data), Some(48));
        assert!(parse_bounded_xcursor_with_limit(&data, 47).is_none());
        assert_eq!(
            parse_bounded_xcursor_with_limit(&data, 48).unwrap().len(),
            3
        );
    }

    #[test]
    fn cursor_file_reader_rejects_oversized_regular_files() {
        let path = test_path("oversized");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(MAX_XCURSOR_FILE_BYTES + 1).unwrap();

        assert!(read_xcursor_file(&path).is_none());

        std::fs::remove_file(path).unwrap();
    }
}
