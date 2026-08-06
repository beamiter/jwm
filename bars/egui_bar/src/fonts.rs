//! The bar's font stack.
//!
//! Pango resolves a missing glyph through fontconfig, so the Cairo bars get
//! Nerd Font icons for free while asking only for a text face like `Lato`.
//! egui has no system fallback: every family that must answer a codepoint has
//! to be registered here, in priority order, or the status cells render as
//! tofu. This is the whole reason the icons used to disappear from this bar.

use egui::{FontData, FontDefinitions, FontFamily};
use font_kit::family_name::FamilyName;
use font_kit::handle::Handle;
use font_kit::properties::Properties;
use font_kit::source::SystemSource;
use log::{info, warn};
use std::sync::Arc;

/// Families carrying the Nerd Font icon range, most preferred first.
///
/// The non-`Mono` variants come first deliberately: their icons keep their
/// natural width instead of being squeezed into one terminal cell, which is
/// what the Cairo bars get from fontconfig and what the pill widths assume.
const ICON_FAMILIES: &[&str] = &[
    "JetBrainsMono Nerd Font",
    "SauceCodePro Nerd Font",
    "DejaVuSansM Nerd Font",
    "Symbols Nerd Font",
    "Symbols Nerd Font Mono",
];

/// CJK coverage for client names, which egui's built-in faces do not have.
const CJK_FAMILIES: &[&str] = &["Noto Sans CJK SC", "Noto Sans CJK TC", "Noto Sans CJK JP"];

/// Install the stack described by a Pango font string such as
/// `"Lato Medium 12"`, with icon and CJK fallbacks behind it.
///
/// Never fails: a bar with a degraded font is worth more than no bar, so a
/// family that cannot be loaded is logged and skipped, leaving egui's own
/// faces to answer for it.
pub fn install(ctx: &egui::Context, description: &str) {
    let source = SystemSource::new();
    let mut fonts = FontDefinitions::default();
    let mut chain = Vec::new();

    let primary = family_candidates(description)
        .into_iter()
        .find_map(|family| register(&mut fonts, &source, &family, "bar-primary"));
    match primary {
        Some(key) => chain.push(key),
        None => warn!("no system font matched {description:?}; falling back to egui's own"),
    }

    match first_available(&mut fonts, &source, ICON_FAMILIES, "bar-icons") {
        Some(key) => chain.push(key),
        None => warn!(
            "no Nerd Font found ({ICON_FAMILIES:?}); status icons will render as missing glyphs"
        ),
    }
    if let Some(key) = first_available(&mut fonts, &source, CJK_FAMILIES, "bar-cjk") {
        chain.push(key);
    }

    // Ahead of egui's defaults, in the order resolved above: egui walks the
    // list per codepoint and takes the first face that has a glyph for it.
    for family in [FontFamily::Proportional, FontFamily::Monospace] {
        let list = fonts.families.entry(family).or_default();
        for (position, key) in chain.iter().enumerate() {
            list.insert(position, key.clone());
        }
    }
    ctx.set_fonts(fonts);
}

fn first_available(
    fonts: &mut FontDefinitions,
    source: &SystemSource,
    families: &[&str],
    key: &str,
) -> Option<String> {
    families
        .iter()
        .find_map(|family| register(fonts, source, family, key))
}

/// Load one family's bytes into `fonts` under `key`, returning the key when it
/// resolved to something.
fn register(
    fonts: &mut FontDefinitions,
    source: &SystemSource,
    family: &str,
    key: &str,
) -> Option<String> {
    let handle = source
        .select_best_match(&[FamilyName::Title(family.to_owned())], &Properties::new())
        .ok()?;
    // The face index matters: the Noto CJK faces ship as `.ttc` collections,
    // and index 0 is a different language's face than the one selected.
    let (bytes, index) = match handle {
        Handle::Path { path, font_index } => (std::fs::read(&path).ok()?, font_index),
        Handle::Memory { bytes, font_index } => (bytes.to_vec(), font_index),
    };
    let mut data = FontData::from_owned(bytes);
    data.index = index;
    fonts.font_data.insert(key.to_owned(), Arc::new(data));
    info!("loaded {family:?} as {key}");
    Some(key.to_owned())
}

/// Family names to try for a Pango font description, most specific first.
///
/// Pango writes `FAMILY [STYLE…] [SIZE]`, and only the family is meaningful
/// here — the presentation config owns the size. Style words are dropped one
/// at a time from the end because a system may register `"Lato Medium"` as its
/// own family (fontconfig does) or only as a weight of `"Lato"`.
fn family_candidates(description: &str) -> Vec<String> {
    const STYLE_WORDS: &[&str] = &[
        "thin",
        "extralight",
        "ultralight",
        "light",
        "semilight",
        "book",
        "regular",
        "medium",
        "semibold",
        "demibold",
        "bold",
        "extrabold",
        "ultrabold",
        "black",
        "heavy",
        "italic",
        "oblique",
        "condensed",
        "expanded",
        "normal",
    ];

    let mut tokens = description.split_whitespace().collect::<Vec<_>>();
    if tokens.last().is_some_and(|last| last.parse::<f32>().is_ok()) {
        tokens.pop();
    }
    if tokens.is_empty() {
        return Vec::new();
    }

    let mut candidates = vec![tokens.join(" ")];
    while tokens.len() > 1
        && tokens
            .last()
            .is_some_and(|last| STYLE_WORDS.contains(&last.to_ascii_lowercase().as_str()))
    {
        tokens.pop();
        candidates.push(tokens.join(" "));
    }
    candidates
}

#[cfg(test)]
mod tests {
    use super::family_candidates;

    #[test]
    fn strips_the_pango_size_and_peels_style_words() {
        assert_eq!(
            family_candidates("Lato Medium 12"),
            vec!["Lato Medium".to_owned(), "Lato".to_owned()]
        );
        assert_eq!(
            family_candidates("Noto Sans CJK SC 11"),
            vec!["Noto Sans CJK SC".to_owned()]
        );
        // A family whose own name ends in a style word still offers itself
        // first, so "Comic Sans Bold" tries the full name before "Comic Sans".
        assert_eq!(
            family_candidates("Comic Sans Bold"),
            vec!["Comic Sans Bold".to_owned(), "Comic Sans".to_owned()]
        );
    }

    #[test]
    fn survives_a_blank_description() {
        assert!(family_candidates("   ").is_empty());
        assert!(family_candidates("13").is_empty());
    }
}
