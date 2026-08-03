//! Backend-independent bitmap text rasterization.
//!
//! Both compositor backends (X11/GLX and Wayland-udev/GLES) need to draw debug
//! HUD/overlay text, and historically each carried its own byte-identical copy of
//! this CPU rasterizer. The pixel output has no GL dependency, so it lives here
//! once; each backend uploads the returned RGBA buffer to a texture in its own
//! API-specific way.

use ab_glyph::{Font, FontArc, GlyphId, ScaleFont, point};
use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex};

static UI_FONTS: LazyLock<Mutex<HashMap<String, Option<FontArc>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Fonts pulled in to cover glyphs the configured UI font lacks — CJK, emoji,
/// anything outside a typical programming face. Kept in discovery order and
/// scanned in memory, so covering a whole script costs one `fc-match` rather
/// than one per character.
static FALLBACK_FONTS: LazyLock<Mutex<Vec<FontArc>>> = LazyLock::new(|| Mutex::new(Vec::new()));

/// Codepoints no installed font could cover. Remembered so a single missing
/// glyph does not spawn `fc-match` on every repaint.
static FALLBACK_MISSES: LazyLock<Mutex<HashSet<u32>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// The fontconfig pattern that asks for any font covering `ch`.
///
/// Split out because it is the one piece of the fallback path that can be
/// tested without knowing which fonts happen to be installed.
fn charset_pattern(ch: char) -> String {
    format!(":charset={:x}", ch as u32)
}

/// Load a font covering `ch`, if fontconfig can find one.
fn load_fallback_font(ch: char) -> Option<FontArc> {
    let path = std::process::Command::new("fc-match")
        .args(["-f", "%{file}\n", &charset_pattern(ch)])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|output| output.lines().next().map(str::trim).map(str::to_owned))
        .filter(|path| !path.is_empty())?;
    let font = FontArc::try_from_vec(std::fs::read(path).ok()?).ok()?;
    // fc-match always answers with *something*; only keep it if it actually
    // covers the character we asked about.
    (font.glyph_id(ch).0 != 0).then_some(font)
}

/// Which font draws a character.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GlyphSource {
    /// The configured UI font covers it.
    Primary,
    /// Index into [`FALLBACK_FONTS`].
    Fallback(usize),
    /// Nothing installed covers it.
    Missing,
}

/// Decide which font draws `ch`, pulling in a fallback the first time a
/// script turns up.
fn source_for_char(primary: &FontArc, ch: char) -> GlyphSource {
    if primary.glyph_id(ch).0 != 0 {
        return GlyphSource::Primary;
    }
    if let Ok(fonts) = FALLBACK_FONTS.lock()
        && let Some(index) = fonts.iter().position(|font| font.glyph_id(ch).0 != 0)
    {
        return GlyphSource::Fallback(index);
    }
    if FALLBACK_MISSES
        .lock()
        .is_ok_and(|misses| misses.contains(&(ch as u32)))
    {
        return GlyphSource::Missing;
    }
    let Some(font) = load_fallback_font(ch) else {
        if let Ok(mut misses) = FALLBACK_MISSES.lock() {
            misses.insert(ch as u32);
        }
        return GlyphSource::Missing;
    };
    let Ok(mut fonts) = FALLBACK_FONTS.lock() else {
        return GlyphSource::Missing;
    };
    fonts.push(font);
    GlyphSource::Fallback(fonts.len() - 1)
}

/// The font behind a source, cloned out of the cache.
fn font_for_source(primary: &FontArc, source: GlyphSource) -> Option<FontArc> {
    match source {
        GlyphSource::Primary | GlyphSource::Missing => Some(primary.clone()),
        GlyphSource::Fallback(index) => FALLBACK_FONTS.lock().ok()?.get(index).cloned(),
    }
}

/// One line resolved to (font index, glyph) pairs, plus the fonts used.
///
/// Resolving up front means the width pass and the draw pass agree on which
/// font drew what, which is what keeps advances and kerning consistent.
struct ResolvedLine {
    glyphs: Vec<(usize, GlyphId)>,
}

fn resolve_line(
    fonts: &mut Vec<FontArc>,
    sources: &mut Vec<GlyphSource>,
    primary: &FontArc,
    line: &str,
) -> ResolvedLine {
    let mut glyphs = Vec::with_capacity(line.chars().count());
    for ch in line.chars() {
        let source = source_for_char(primary, ch);
        // Sources are compared, not font handles: `FontArc` has no pointer
        // identity to compare, and the source *is* the identity.
        let index = if let Some(index) = sources.iter().position(|known| *known == source) {
            index
        } else {
            let Some(font) = font_for_source(primary, source) else {
                continue;
            };
            fonts.push(font);
            sources.push(source);
            fonts.len() - 1
        };
        let id = match source {
            // Nothing installed covers it: a question mark in the primary
            // font is still better than a blank.
            GlyphSource::Missing => primary.glyph_id('?'),
            _ => fonts[index].glyph_id(ch),
        };
        glyphs.push((index, id));
    }
    ResolvedLine { glyphs }
}

/// Extract the conventional trailing point size from a fontconfig pattern.
/// `SauceCodePro Nerd Font Regular 11` becomes 11pt (about 18 physical px for
/// the system UI); patterns without a size use the modern 18px default.
pub(crate) fn ui_font_pixel_size(description: &str) -> f32 {
    description
        .split_whitespace()
        .next_back()
        .and_then(|s| s.parse::<f32>().ok())
        .map(|pt| (pt * 1.6).clamp(14.0, 32.0))
        .unwrap_or(18.0)
}

fn ui_font_family(description: &str) -> String {
    let mut parts: Vec<&str> = description.split_whitespace().collect();
    if parts.last().is_some_and(|part| part.parse::<f32>().is_ok()) {
        parts.pop();
    }
    while parts.last().is_some_and(|part| {
        matches!(
            part.to_ascii_lowercase().as_str(),
            "regular" | "book" | "medium" | "semibold" | "bold" | "italic" | "oblique"
        )
    }) {
        parts.pop();
    }
    if parts.is_empty() {
        "monospace".to_owned()
    } else {
        parts.join(" ")
    }
}

fn load_ui_font(description: &str) -> Option<FontArc> {
    if let Some(cached) = UI_FONTS.lock().ok()?.get(description).cloned() {
        return cached;
    }
    let family = ui_font_family(description);
    let path = std::process::Command::new("fc-match")
        .args(["-f", "%{file}\n", "--", &family])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|output| output.lines().next().map(str::trim).map(str::to_owned))
        .filter(|path| !path.is_empty());
    let font = path
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|bytes| FontArc::try_from_vec(bytes).ok());
    if let Ok(mut fonts) = UI_FONTS.lock() {
        fonts.insert(description.to_owned(), font.clone());
    }
    font
}

/// Transparent margin the rasterizer leaves on every side, so an overhanging
/// glyph edge is not clipped by the texture bounds.
const TEXT_PAD: u32 = 2;

/// Advance width of one resolved line, kerning included.
///
/// Shared by the rasterizer and by [`measure_ui_text_width`] so a caller that
/// asks how wide a string will be gets the number the raster then produces.
fn resolved_line_width(fonts: &[FontArc], line: &ResolvedLine, pixel_size: f32) -> f32 {
    let mut width = 0.0;
    let mut previous: Option<(usize, GlyphId)> = None;
    for &(font_index, id) in &line.glyphs {
        let scaled = fonts[font_index].as_scaled(pixel_size);
        // Kerning is a property of one font's pairs; across a fallback
        // boundary there is no pair to look up.
        if let Some((prev_index, prev_id)) = previous
            && prev_index == font_index
        {
            width += scaled.kern(prev_id, id);
        }
        width += scaled.h_advance(id);
        previous = Some((font_index, id));
    }
    width
}

/// Width in pixels [`render_ui_text_to_rgba`] would return for `text`, without
/// rasterizing it. Single line: callers that fit text to a box measure one
/// candidate after another, and outlining every glyph of each would cost far
/// more than the search.
pub(crate) fn measure_ui_text_width(text: &str, font_description: &str, pixel_size: f32) -> u32 {
    if text.is_empty() {
        return 0;
    }
    let Some(font) = load_ui_font(font_description) else {
        // Matches the bitmap fallback the rasterizer drops to.
        return text.chars().count() as u32 * GLYPH_W * 2;
    };
    let mut fonts: Vec<FontArc> = Vec::with_capacity(2);
    let mut sources: Vec<GlyphSource> = Vec::with_capacity(2);
    let line = resolve_line(&mut fonts, &mut sources, &font, text);
    (resolved_line_width(&fonts, &line, pixel_size).ceil() as u32).saturating_add(TEXT_PAD * 2)
}

/// The longest prefix of `text` that fits `max_width` pixels, with an ellipsis
/// standing in for what was cut.
///
/// Truncating by character count would cut a proportional face in the wrong
/// place — `WWWWW` and `iiiii` are not the same width — so the cut is found by
/// measuring, on character boundaries so a multi-byte glyph is never split.
/// Returns an empty string when not even the ellipsis fits.
///
/// The result is always one line: a window title carrying a newline would
/// otherwise be measured as one line and rasterized as two, and the second one
/// would hang below whatever box the caller sized from the measurement.
pub(crate) fn fit_ui_text(
    text: &str,
    font_description: &str,
    pixel_size: f32,
    max_width: u32,
) -> String {
    const ELLIPSIS: char = '…';

    let collapsed: String = {
        let mut out = String::with_capacity(text.len());
        let mut in_space = false;
        for ch in text.chars() {
            if ch.is_whitespace() || ch.is_control() {
                in_space = true;
                continue;
            }
            if in_space && !out.is_empty() {
                out.push(' ');
            }
            in_space = false;
            out.push(ch);
        }
        out
    };
    let text = collapsed.as_str();
    if text.is_empty() || max_width == 0 {
        return String::new();
    }
    if measure_ui_text_width(text, font_description, pixel_size) <= max_width {
        return text.to_owned();
    }

    let boundaries: Vec<usize> = text
        .char_indices()
        .map(|(index, _)| index)
        .skip(1)
        .chain(std::iter::once(text.len()))
        .collect();

    // Binary search the last prefix that still fits once the ellipsis is on
    // it. `fits` is monotone in the prefix length, so this converges on the
    // longest one in a handful of measurements.
    let mut low = 0usize;
    let mut high = boundaries.len();
    let mut best = String::new();
    while low < high {
        let middle = (low + high).div_ceil(2);
        if middle == 0 {
            break;
        }
        let mut candidate = text[..boundaries[middle - 1]].trim_end().to_owned();
        candidate.push(ELLIPSIS);
        if measure_ui_text_width(&candidate, font_description, pixel_size) <= max_width {
            best = candidate;
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    best
}

/// Render UTF-8 UI text with the configured TrueType/OpenType font. This path
/// supports Nerd Font private-use glyphs and produces smooth alpha coverage.
/// The embedded bitmap font remains the dependency-free fallback.
pub(crate) fn render_ui_text_to_rgba(
    text: &str,
    font_description: &str,
    pixel_size: f32,
    fg: [u8; 4],
) -> (Vec<u8>, u32, u32) {
    let Some(font) = load_ui_font(font_description) else {
        return render_text_to_rgba(text, 2, fg);
    };
    if text.is_empty() {
        return (Vec::new(), 0, 0);
    }

    // Line metrics come from the configured font even when a fallback draws a
    // glyph, so rows stay on the same baseline whatever script they mix.
    let scaled = font.as_scaled(pixel_size);
    let ascent = scaled.ascent();
    let line_height = (ascent - scaled.descent() + scaled.line_gap())
        .ceil()
        .max(1.0);
    let lines: Vec<&str> = text.lines().collect();
    let mut fonts: Vec<FontArc> = Vec::with_capacity(2);
    let mut sources: Vec<GlyphSource> = Vec::with_capacity(2);
    let resolved: Vec<ResolvedLine> = lines
        .iter()
        .map(|line| resolve_line(&mut fonts, &mut sources, &font, line))
        .collect();

    let advance =
        |font_index: usize, id: GlyphId| fonts[font_index].as_scaled(pixel_size).h_advance(id);
    // Kerning is a property of one font's pairs; across a fallback boundary
    // there is no pair to look up.
    let kern = |previous: Option<(usize, GlyphId)>, font_index: usize, id: GlyphId| match previous {
        Some((prev_index, prev_id)) if prev_index == font_index => {
            fonts[font_index].as_scaled(pixel_size).kern(prev_id, id)
        }
        _ => 0.0,
    };

    let width = resolved
        .iter()
        .map(|line| resolved_line_width(&fonts, line, pixel_size).ceil() as u32)
        .max()
        .unwrap_or(0)
        .saturating_add(TEXT_PAD * 2);
    let height = (line_height * lines.len().max(1) as f32).ceil() as u32 + TEXT_PAD * 2;
    if width == 0 || height == 0 {
        return (Vec::new(), 0, 0);
    }
    let mut pixels = vec![0u8; width as usize * height as usize * 4];
    for (line_index, line) in resolved.iter().enumerate() {
        let baseline = 2.0 + ascent + line_index as f32 * line_height;
        let mut x = 2.0;
        let mut previous = None;
        for &(font_index, id) in &line.glyphs {
            x += kern(previous, font_index, id);
            let glyph = id.with_scale_and_position(pixel_size, point(x, baseline));
            if let Some(outlined) = fonts[font_index].outline_glyph(glyph) {
                let bounds = outlined.px_bounds();
                outlined.draw(|gx, gy, coverage| {
                    let px = bounds.min.x as i32 + gx as i32;
                    let py = bounds.min.y as i32 + gy as i32;
                    if px < 0 || py < 0 || px >= width as i32 || py >= height as i32 {
                        return;
                    }
                    let offset = (py as usize * width as usize + px as usize) * 4;
                    let alpha = (coverage * fg[3] as f32).round().clamp(0.0, 255.0) as u8;
                    if alpha > pixels[offset + 3] {
                        pixels[offset..offset + 3].copy_from_slice(&fg[..3]);
                        pixels[offset + 3] = alpha;
                    }
                });
            }
            x += advance(font_index, id);
            previous = Some((font_index, id));
        }
    }
    (pixels, width, height)
}

/// Glyph width in pixels (before scaling).
pub(crate) const GLYPH_W: u32 = 6;
/// Glyph height in pixels (before scaling).
pub(crate) const GLYPH_H: u32 = 10;

/// Simple 6x10 bitmap font for printable ASCII (32..=126).
/// Each character is 6 columns x 10 rows, stored as 10 bytes (1 byte per row,
/// top 6 bits used). Non-ASCII characters are rendered as '?'.
#[rustfmt::skip]
pub(crate) const FONT_6X10: [u8; 95 * 10] = [
    // Space (32)
    0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,
    // ! (33)
    0x20,0x20,0x20,0x20,0x20,0x20,0x00,0x20,0x00,0x00,
    // " (34)
    0x50,0x50,0x50,0x00,0x00,0x00,0x00,0x00,0x00,0x00,
    // # (35)
    0x50,0x50,0xF8,0x50,0xF8,0x50,0x50,0x00,0x00,0x00,
    // $ (36)
    0x20,0x78,0xA0,0x70,0x28,0xF0,0x20,0x00,0x00,0x00,
    // % (37)
    0xC0,0xC8,0x10,0x20,0x40,0x98,0x18,0x00,0x00,0x00,
    // & (38)
    0x60,0x90,0xA0,0x40,0xA8,0x90,0x68,0x00,0x00,0x00,
    // ' (39)
    0x60,0x20,0x40,0x00,0x00,0x00,0x00,0x00,0x00,0x00,
    // ( (40)
    0x10,0x20,0x40,0x40,0x40,0x20,0x10,0x00,0x00,0x00,
    // ) (41)
    0x40,0x20,0x10,0x10,0x10,0x20,0x40,0x00,0x00,0x00,
    // * (42)
    0x00,0x20,0xA8,0x70,0xA8,0x20,0x00,0x00,0x00,0x00,
    // + (43)
    0x00,0x20,0x20,0xF8,0x20,0x20,0x00,0x00,0x00,0x00,
    // , (44)
    0x00,0x00,0x00,0x00,0x00,0x60,0x20,0x40,0x00,0x00,
    // - (45)
    0x00,0x00,0x00,0xF8,0x00,0x00,0x00,0x00,0x00,0x00,
    // . (46)
    0x00,0x00,0x00,0x00,0x00,0x60,0x60,0x00,0x00,0x00,
    // / (47)
    0x00,0x08,0x10,0x20,0x40,0x80,0x00,0x00,0x00,0x00,
    // 0 (48)
    0x70,0x88,0x98,0xA8,0xC8,0x88,0x70,0x00,0x00,0x00,
    // 1 (49)
    0x20,0x60,0x20,0x20,0x20,0x20,0x70,0x00,0x00,0x00,
    // 2 (50)
    0x70,0x88,0x08,0x10,0x20,0x40,0xF8,0x00,0x00,0x00,
    // 3 (51)
    0xF8,0x10,0x20,0x10,0x08,0x88,0x70,0x00,0x00,0x00,
    // 4 (52)
    0x10,0x30,0x50,0x90,0xF8,0x10,0x10,0x00,0x00,0x00,
    // 5 (53)
    0xF8,0x80,0xF0,0x08,0x08,0x88,0x70,0x00,0x00,0x00,
    // 6 (54)
    0x30,0x40,0x80,0xF0,0x88,0x88,0x70,0x00,0x00,0x00,
    // 7 (55)
    0xF8,0x08,0x10,0x20,0x40,0x40,0x40,0x00,0x00,0x00,
    // 8 (56)
    0x70,0x88,0x88,0x70,0x88,0x88,0x70,0x00,0x00,0x00,
    // 9 (57)
    0x70,0x88,0x88,0x78,0x08,0x10,0x60,0x00,0x00,0x00,
    // : (58)
    0x00,0x60,0x60,0x00,0x60,0x60,0x00,0x00,0x00,0x00,
    // ; (59)
    0x00,0x60,0x60,0x00,0x60,0x20,0x40,0x00,0x00,0x00,
    // < (60)
    0x10,0x20,0x40,0x80,0x40,0x20,0x10,0x00,0x00,0x00,
    // = (61)
    0x00,0x00,0xF8,0x00,0xF8,0x00,0x00,0x00,0x00,0x00,
    // > (62)
    0x40,0x20,0x10,0x08,0x10,0x20,0x40,0x00,0x00,0x00,
    // ? (63)
    0x70,0x88,0x08,0x10,0x20,0x00,0x20,0x00,0x00,0x00,
    // @ (64)
    0x70,0x88,0xB8,0xA8,0xB8,0x80,0x70,0x00,0x00,0x00,
    // A (65)
    0x70,0x88,0x88,0xF8,0x88,0x88,0x88,0x00,0x00,0x00,
    // B (66)
    0xF0,0x88,0x88,0xF0,0x88,0x88,0xF0,0x00,0x00,0x00,
    // C (67)
    0x70,0x88,0x80,0x80,0x80,0x88,0x70,0x00,0x00,0x00,
    // D (68)
    0xE0,0x90,0x88,0x88,0x88,0x90,0xE0,0x00,0x00,0x00,
    // E (69)
    0xF8,0x80,0x80,0xF0,0x80,0x80,0xF8,0x00,0x00,0x00,
    // F (70)
    0xF8,0x80,0x80,0xF0,0x80,0x80,0x80,0x00,0x00,0x00,
    // G (71)
    0x70,0x88,0x80,0xB8,0x88,0x88,0x78,0x00,0x00,0x00,
    // H (72)
    0x88,0x88,0x88,0xF8,0x88,0x88,0x88,0x00,0x00,0x00,
    // I (73)
    0x70,0x20,0x20,0x20,0x20,0x20,0x70,0x00,0x00,0x00,
    // J (74)
    0x38,0x10,0x10,0x10,0x10,0x90,0x60,0x00,0x00,0x00,
    // K (75)
    0x88,0x90,0xA0,0xC0,0xA0,0x90,0x88,0x00,0x00,0x00,
    // L (76)
    0x80,0x80,0x80,0x80,0x80,0x80,0xF8,0x00,0x00,0x00,
    // M (77)
    0x88,0xD8,0xA8,0xA8,0x88,0x88,0x88,0x00,0x00,0x00,
    // N (78)
    0x88,0xC8,0xA8,0x98,0x88,0x88,0x88,0x00,0x00,0x00,
    // O (79)
    0x70,0x88,0x88,0x88,0x88,0x88,0x70,0x00,0x00,0x00,
    // P (80)
    0xF0,0x88,0x88,0xF0,0x80,0x80,0x80,0x00,0x00,0x00,
    // Q (81)
    0x70,0x88,0x88,0x88,0xA8,0x90,0x68,0x00,0x00,0x00,
    // R (82)
    0xF0,0x88,0x88,0xF0,0xA0,0x90,0x88,0x00,0x00,0x00,
    // S (83)
    0x78,0x80,0x80,0x70,0x08,0x08,0xF0,0x00,0x00,0x00,
    // T (84)
    0xF8,0x20,0x20,0x20,0x20,0x20,0x20,0x00,0x00,0x00,
    // U (85)
    0x88,0x88,0x88,0x88,0x88,0x88,0x70,0x00,0x00,0x00,
    // V (86)
    0x88,0x88,0x88,0x88,0x88,0x50,0x20,0x00,0x00,0x00,
    // W (87)
    0x88,0x88,0x88,0xA8,0xA8,0xD8,0x88,0x00,0x00,0x00,
    // X (88)
    0x88,0x88,0x50,0x20,0x50,0x88,0x88,0x00,0x00,0x00,
    // Y (89)
    0x88,0x88,0x50,0x20,0x20,0x20,0x20,0x00,0x00,0x00,
    // Z (90)
    0xF8,0x08,0x10,0x20,0x40,0x80,0xF8,0x00,0x00,0x00,
    // [ (91)
    0x70,0x40,0x40,0x40,0x40,0x40,0x70,0x00,0x00,0x00,
    // \ (92)
    0x00,0x80,0x40,0x20,0x10,0x08,0x00,0x00,0x00,0x00,
    // ] (93)
    0x70,0x10,0x10,0x10,0x10,0x10,0x70,0x00,0x00,0x00,
    // ^ (94)
    0x20,0x50,0x88,0x00,0x00,0x00,0x00,0x00,0x00,0x00,
    // _ (95)
    0x00,0x00,0x00,0x00,0x00,0x00,0xF8,0x00,0x00,0x00,
    // ` (96)
    0x40,0x20,0x10,0x00,0x00,0x00,0x00,0x00,0x00,0x00,
    // a (97)
    0x00,0x00,0x70,0x08,0x78,0x88,0x78,0x00,0x00,0x00,
    // b (98)
    0x80,0x80,0xB0,0xC8,0x88,0x88,0xF0,0x00,0x00,0x00,
    // c (99)
    0x00,0x00,0x70,0x80,0x80,0x88,0x70,0x00,0x00,0x00,
    // d (100)
    0x08,0x08,0x68,0x98,0x88,0x88,0x78,0x00,0x00,0x00,
    // e (101)
    0x00,0x00,0x70,0x88,0xF8,0x80,0x70,0x00,0x00,0x00,
    // f (102)
    0x30,0x48,0x40,0xE0,0x40,0x40,0x40,0x00,0x00,0x00,
    // g (103)
    0x00,0x00,0x78,0x88,0x78,0x08,0x70,0x00,0x00,0x00,
    // h (104)
    0x80,0x80,0xB0,0xC8,0x88,0x88,0x88,0x00,0x00,0x00,
    // i (105)
    0x20,0x00,0x60,0x20,0x20,0x20,0x70,0x00,0x00,0x00,
    // j (106)
    0x10,0x00,0x30,0x10,0x10,0x90,0x60,0x00,0x00,0x00,
    // k (107)
    0x80,0x80,0x90,0xA0,0xC0,0xA0,0x90,0x00,0x00,0x00,
    // l (108)
    0x60,0x20,0x20,0x20,0x20,0x20,0x70,0x00,0x00,0x00,
    // m (109)
    0x00,0x00,0xD0,0xA8,0xA8,0x88,0x88,0x00,0x00,0x00,
    // n (110)
    0x00,0x00,0xB0,0xC8,0x88,0x88,0x88,0x00,0x00,0x00,
    // o (111)
    0x00,0x00,0x70,0x88,0x88,0x88,0x70,0x00,0x00,0x00,
    // p (112)
    0x00,0x00,0xF0,0x88,0xF0,0x80,0x80,0x00,0x00,0x00,
    // q (113)
    0x00,0x00,0x68,0x98,0x78,0x08,0x08,0x00,0x00,0x00,
    // r (114)
    0x00,0x00,0xB0,0xC8,0x80,0x80,0x80,0x00,0x00,0x00,
    // s (115)
    0x00,0x00,0x78,0x80,0x70,0x08,0xF0,0x00,0x00,0x00,
    // t (116)
    0x40,0x40,0xE0,0x40,0x40,0x48,0x30,0x00,0x00,0x00,
    // u (117)
    0x00,0x00,0x88,0x88,0x88,0x98,0x68,0x00,0x00,0x00,
    // v (118)
    0x00,0x00,0x88,0x88,0x88,0x50,0x20,0x00,0x00,0x00,
    // w (119)
    0x00,0x00,0x88,0x88,0xA8,0xA8,0x50,0x00,0x00,0x00,
    // x (120)
    0x00,0x00,0x88,0x50,0x20,0x50,0x88,0x00,0x00,0x00,
    // y (121)
    0x00,0x00,0x88,0x88,0x78,0x08,0x70,0x00,0x00,0x00,
    // z (122)
    0x00,0x00,0xF8,0x10,0x20,0x40,0xF8,0x00,0x00,0x00,
    // { (123)
    0x10,0x20,0x20,0x40,0x20,0x20,0x10,0x00,0x00,0x00,
    // | (124)
    0x20,0x20,0x20,0x20,0x20,0x20,0x20,0x00,0x00,0x00,
    // } (125)
    0x40,0x20,0x20,0x10,0x20,0x20,0x40,0x00,0x00,0x00,
    // ~ (126)
    0x00,0x40,0xA8,0x10,0x00,0x00,0x00,0x00,0x00,0x00,
];

/// Rasterize a multi-line ASCII string into an RGBA pixel buffer.
///
/// Returns `(pixels, width, height)`. Transparent background, foreground
/// colour set by `fg`. Each glyph is rendered at `scale`x magnification
/// (e.g. `scale = 2` gives 12x20 effective pixels per character).
pub(crate) fn render_text_to_rgba(text: &str, scale: u32, fg: [u8; 4]) -> (Vec<u8>, u32, u32) {
    let char_w = GLYPH_W * scale;
    let char_h = GLYPH_H * scale;

    let lines: Vec<&str> = text.split('\n').collect();
    let max_cols = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0) as u32;
    let num_lines = lines.len() as u32;

    let img_w = max_cols * char_w;
    let img_h = num_lines * char_h;
    if img_w == 0 || img_h == 0 {
        return (Vec::new(), 0, 0);
    }

    let mut pixels = vec![0u8; (img_w * img_h * 4) as usize];

    for (line_idx, line) in lines.iter().enumerate() {
        for (ci, ch) in line.chars().enumerate() {
            let glyph_idx = if (32..=126).contains(&(ch as u32)) {
                (ch as u32 - 32) as usize
            } else {
                ('?' as u32 - 32) as usize
            };
            let glyph = &FONT_6X10[glyph_idx * 10..(glyph_idx + 1) * 10];

            let base_x = ci as u32 * char_w;
            let base_y = line_idx as u32 * char_h;

            for row in 0..GLYPH_H {
                let bits = glyph[row as usize];
                for col in 0..GLYPH_W {
                    if bits & (0x80 >> col) != 0 {
                        for sy in 0..scale {
                            for sx in 0..scale {
                                let px = base_x + col * scale + sx;
                                let py = base_y + row * scale + sy;
                                if px < img_w && py < img_h {
                                    let idx = ((py * img_w + px) * 4) as usize;
                                    pixels[idx] = fg[0];
                                    pixels[idx + 1] = fg[1];
                                    pixels[idx + 2] = fg[2];
                                    pixels[idx + 3] = fg[3];
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    (pixels, img_w, img_h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_string_yields_no_pixels() {
        let (px, w, h) = render_text_to_rgba("", 2, [255, 255, 255, 255]);
        assert!(px.is_empty());
        assert_eq!((w, h), (0, 0));
    }

    /// The font behind these is whatever the machine has, so the assertions
    /// are about the contract rather than about pixel counts.
    const FIT_FONT: &str = "monospace 11";

    #[test]
    fn text_that_fits_is_returned_whole() {
        let text = "kitty";
        let width = measure_ui_text_width(text, FIT_FONT, 12.0);
        assert_eq!(fit_ui_text(text, FIT_FONT, 12.0, width), text);
        assert_eq!(fit_ui_text(text, FIT_FONT, 12.0, width + 100), text);
    }

    #[test]
    fn a_long_title_is_cut_to_the_budget_with_an_ellipsis() {
        let text = "a very long window title that no tab cell could ever hold";
        let full = measure_ui_text_width(text, FIT_FONT, 12.0);
        let budget = full / 4;
        let fitted = fit_ui_text(text, FIT_FONT, 12.0, budget);
        assert!(fitted.len() < text.len());
        assert!(fitted.ends_with('…'), "{fitted:?} should be elided");
        assert!(measure_ui_text_width(&fitted, FIT_FONT, 12.0) <= budget);
    }

    #[test]
    fn a_budget_too_small_for_anything_yields_nothing() {
        assert_eq!(fit_ui_text("anything", FIT_FONT, 12.0, 0), "");
        assert_eq!(fit_ui_text("", FIT_FONT, 12.0, 500), "");
        assert_eq!(fit_ui_text("   ", FIT_FONT, 12.0, 500), "");
    }

    /// A title is drawn on one line and measured on one line, so the newline a
    /// client is free to put in `WM_NAME` has to be gone before either.
    #[test]
    fn whitespace_is_collapsed_onto_a_single_line() {
        assert_eq!(
            fit_ui_text("  two\n\tlines  ", FIT_FONT, 12.0, 100_000),
            "two lines"
        );
    }

    #[test]
    fn dimensions_track_glyph_grid_and_scale() {
        let (px, w, h) = render_text_to_rgba("AB", 3, [1, 2, 3, 4]);
        assert_eq!(w, 2 * GLYPH_W * 3);
        assert_eq!(h, GLYPH_H * 3);
        assert_eq!(px.len() as u32, w * h * 4);
    }

    #[test]
    fn xft_style_font_description_splits_family_and_size() {
        assert_eq!(
            ui_font_family("SauceCodePro Nerd Font Regular 11"),
            "SauceCodePro Nerd Font"
        );
        assert_eq!(
            ui_font_pixel_size("SauceCodePro Nerd Font Regular 11"),
            17.6
        );
    }

    #[test]
    fn multiline_uses_widest_line_and_line_count() {
        let (_, w, h) = render_text_to_rgba("a\nbcd", 1, [0; 4]);
        assert_eq!(w, 3 * GLYPH_W); // widest line is "bcd"
        assert_eq!(h, 2 * GLYPH_H); // two lines
    }

    #[test]
    fn set_pixels_use_foreground_color_others_transparent() {
        // 'A' has set bits, space does not.
        let fg = [10, 20, 30, 40];
        let (px, _, _) = render_text_to_rgba("A", 1, fg);
        assert!(
            px.chunks_exact(4).any(|p| p == fg),
            "expected some fg pixels"
        );
        assert!(
            px.chunks_exact(4).any(|p| p == [0, 0, 0, 0]),
            "expected some transparent pixels"
        );
    }

    #[test]
    fn charset_patterns_ask_fontconfig_by_codepoint() {
        assert_eq!(charset_pattern('\u{4e2d}'), ":charset=4e2d");
        assert_eq!(charset_pattern('A'), ":charset=41");
        // Astral-plane characters (emoji) use their full codepoint.
        assert_eq!(charset_pattern('\u{1f600}'), ":charset=1f600");
    }

    #[test]
    fn glyph_sources_are_comparable_identities() {
        // Resolution keys fonts by source rather than by handle, so the
        // comparison has to distinguish the slots.
        assert_eq!(GlyphSource::Primary, GlyphSource::Primary);
        assert_ne!(GlyphSource::Primary, GlyphSource::Fallback(0));
        assert_ne!(GlyphSource::Fallback(0), GlyphSource::Fallback(1));
        assert_ne!(GlyphSource::Missing, GlyphSource::Primary);
    }

    #[test]
    fn non_ascii_falls_back_to_question_mark() {
        // A lone '?' and a non-ASCII char must rasterize to the same glyph.
        let (q, _, _) = render_text_to_rgba("?", 1, [255; 4]);
        let (non_ascii, _, _) = render_text_to_rgba("\u{2603}", 1, [255; 4]); // snowman
        assert_eq!(q, non_ascii);
    }
}
