// src/backend/xcursor_theme.rs
//
// Shared Xcursor theme handling. Backends that render or install a real pointer
// (the Wayland DRM/KMS software cursor and the X11RB/XCB RENDER cursors) load
// their images through this module so a single `[appearance]` cursor_theme /
// cursor_size configuration drives every backend consistently — the macOS-style
// "one pointer for the whole session" behavior.

use std::collections::HashMap;

use xcursor::{
    CursorTheme,
    parser::{Image, parse_xcursor},
};

use crate::backend::common_define::StdCursorKind;

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
        .min_by_key(|img| (target_size as i32 - img.size as i32).abs())?;
    images
        .iter()
        .find(|img| img.width == nearest.width && img.height == nearest.height)
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
                    let mut file = std::fs::File::open(path).ok()?;
                    let mut data = Vec::new();
                    std::io::Read::read_to_end(&mut file, &mut data).ok()?;
                    parse_xcursor(&data)
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
