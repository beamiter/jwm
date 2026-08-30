//! Deterministic icon-glyph font selection.
//!
//! Bar labels mix ordinary text with Nerd Font glyphs from the Unicode private
//! use area. A UI font — `font = "Lato Medium 12"` — contains none of them, so
//! Pango resolves each glyph by falling back through the fontconfig sort. The
//! private use area is unassigned, which means *any* installed font may claim
//! those code points and none of them agree on what they draw: on a stock
//! Ubuntu desktop `U+F013` (gear) and `U+F015` (home) both resolve to Arial,
//! whose private range holds unrelated shapes, so those two tag pills came out
//! as a stray tick and an empty box. The fallback sort is also sensitive to the
//! session's fontconfig state, which is how one more tag glyph broke under one
//! window-manager backend and not another.
//!
//! Nothing about that is fixable by picking different code points — the next
//! glyph is one fontconfig cache away from the same fate. What fixes it is
//! naming the icon font explicitly: Pango accepts a comma-separated family list
//! in a font description and tries the families in order, so appending an
//! installed Nerd Font sends every private-use glyph there while ordinary text
//! keeps the UI family in front.

/// Substring that identifies a patched icon font family, matched
/// case-insensitively.
const NERD_FONT_MARKER: &str = "nerd font";

/// Icon families tried first, in order, when the configuration names none.
///
/// The `Symbols` builds are the ones packaged purely as a glyph source, so a
/// desktop that has one is telling us exactly which font to use; anything else
/// is a text font that happens to carry the same glyphs.
pub const PREFERRED_ICON_FAMILIES: [&str; 3] = [
    "Symbols Nerd Font Mono",
    "Symbols Nerd Font",
    "Symbols Nerd Font Propo",
];

/// Choose the family that should back private-use glyphs.
///
/// `configured` wins whenever it is non-empty — a family named by hand is a
/// decision, not a hint, and it is honoured even when this process cannot see
/// it in `available` (fontconfig substitution is then the user's business).
/// Otherwise the first installed entry of [`PREFERRED_ICON_FAMILIES`] wins, and
/// failing that the alphabetically first family whose name contains
/// `Nerd Font`. Sorting matters more than the specific winner: every bar on the
/// machine must resolve the same glyph to the same font on every start.
#[must_use]
pub fn select_icon_family<'a, I>(available: I, configured: Option<&str>) -> Option<String>
where
    I: IntoIterator<Item = &'a str>,
{
    if let Some(configured) = configured.map(str::trim).filter(|name| !name.is_empty()) {
        return Some(configured.to_owned());
    }

    let mut preferred: Option<(usize, &str)> = None;
    let mut fallback: Option<&str> = None;
    for family in available {
        if !is_nerd_font_family(family) {
            continue;
        }
        if fallback.is_none_or(|current| family < current) {
            fallback = Some(family);
        }
        let Some(rank) = PREFERRED_ICON_FAMILIES
            .iter()
            .position(|candidate| family.eq_ignore_ascii_case(candidate))
        else {
            continue;
        };
        if preferred.is_none_or(|current| (rank, family) < current) {
            preferred = Some((rank, family));
        }
    }
    preferred
        .map(|(_, family)| family)
        .or(fallback)
        .map(str::to_owned)
}

/// Choose a Nerd Font family only when that family is actually installed.
///
/// [`select_icon_family`] deliberately treats an explicit configuration value
/// as authoritative, even when the caller's font map cannot currently see it.
/// That is useful for renderers which want to leave fontconfig substitution to
/// the user, but it is not a sufficient gate for switching a presentation from
/// portable emoji to private-use Nerd Font codepoints: without the requested
/// font those codepoints can resolve to unrelated glyphs.  This stricter helper
/// returns the canonical installed family name, or `None` when the selected
/// family is absent or is not a Nerd Font family.
#[must_use]
pub fn select_installed_nerd_font_family<'a, I>(
    available: I,
    configured: Option<&str>,
) -> Option<String>
where
    I: IntoIterator<Item = &'a str>,
{
    if let Some(configured) = configured.map(str::trim).filter(|name| !name.is_empty()) {
        if !is_nerd_font_family(configured) {
            return None;
        }
        return available
            .into_iter()
            .find(|family| family.eq_ignore_ascii_case(configured))
            .map(str::to_owned);
    }
    select_icon_family(available, None)
}

fn is_nerd_font_family(family: &str) -> bool {
    family
        .as_bytes()
        .windows(NERD_FONT_MARKER.len())
        .any(|window| window.eq_ignore_ascii_case(NERD_FONT_MARKER.as_bytes()))
}

/// Append `icon` to a Pango family list, keeping `base` in front.
///
/// A family already present anywhere in `base` is not repeated, so re-applying
/// this to an already-composed description is a no-op rather than a growing
/// list. An empty `base` yields the icon family alone: a description with no
/// family at all would otherwise lose its glyphs to the default sort again.
#[must_use]
pub fn compose_family_list(base: &str, icon: &str) -> String {
    let icon = icon.trim();
    if icon.is_empty() {
        return base.to_owned();
    }
    let already_listed = base
        .split(',')
        .any(|family| family.trim().eq_ignore_ascii_case(icon));
    if already_listed {
        return base.to_owned();
    }
    if base.trim().is_empty() {
        return icon.to_owned();
    }
    format!("{base},{icon}")
}

/// Pango-side integration: enumerate the installed families and apply the
/// resolved icon font to a font description.
#[cfg(feature = "render-cairo")]
mod pango_integration {
    use pango::FontDescription;
    use pango::prelude::{FontFamilyExt as _, FontMapExt as _};

    /// Families this process can actually use, as reported by the shared
    /// Pango-Cairo font map.
    #[must_use]
    pub fn installed_families() -> Vec<String> {
        pangocairo::FontMap::default()
            .list_families()
            .iter()
            .map(|family| family.name().to_string())
            .collect()
    }

    /// Return `font` with an icon family appended to its family list.
    ///
    /// The description is returned unchanged when no icon font is configured
    /// and none is installed: there is then nothing better to point the glyphs
    /// at, and rewriting the family list would only hide that from the caller.
    #[must_use]
    pub fn with_icon_fallback(font: &FontDescription, configured: Option<&str>) -> FontDescription {
        let families = installed_families();
        let Some(icon) = super::select_icon_family(families.iter().map(String::as_str), configured)
        else {
            return font.clone();
        };
        let base = font.family().unwrap_or_default();
        let composed = super::compose_family_list(base.as_str(), &icon);
        let mut font = font.clone();
        font.set_family(&composed);
        font
    }
}

#[cfg(feature = "render-cairo")]
pub use pango_integration::{installed_families, with_icon_fallback};

#[cfg(test)]
mod tests {
    use super::*;

    const INSTALLED: [&str; 6] = [
        "Lato",
        "DejaVuSansM Nerd Font",
        "Arial",
        "JetBrainsMono Nerd Font Mono",
        "Symbols Nerd Font Mono",
        "Noto Sans",
    ];

    #[test]
    fn a_configured_family_wins_even_when_this_process_cannot_see_it() {
        assert_eq!(
            select_icon_family(INSTALLED, Some("  Iosevka Nerd Font  ")),
            Some("Iosevka Nerd Font".to_owned())
        );
        // Blank configuration is absence, not a family named "".
        assert_eq!(
            select_icon_family(INSTALLED, Some("   ")),
            Some("Symbols Nerd Font Mono".to_owned())
        );
    }

    #[test]
    fn the_dedicated_symbols_build_outranks_a_text_font_carrying_the_same_glyphs() {
        assert_eq!(
            select_icon_family(INSTALLED, None),
            Some("Symbols Nerd Font Mono".to_owned())
        );
    }

    #[test]
    fn selection_is_stable_regardless_of_enumeration_order() {
        let forward = ["DejaVuSansM Nerd Font", "JetBrainsMono Nerd Font", "Arial"];
        let reversed = ["Arial", "JetBrainsMono Nerd Font", "DejaVuSansM Nerd Font"];
        assert_eq!(
            select_icon_family(forward, None),
            select_icon_family(reversed, None)
        );
        assert_eq!(
            select_icon_family(forward, None),
            Some("DejaVuSansM Nerd Font".to_owned())
        );
    }

    #[test]
    fn a_desktop_without_a_patched_font_reports_nothing_to_point_glyphs_at() {
        assert_eq!(select_icon_family(["Lato", "Arial"], None), None);
        assert_eq!(select_icon_family([], None), None);
    }

    #[test]
    fn installed_selection_rejects_an_unavailable_explicit_family() {
        assert_eq!(
            select_installed_nerd_font_family(INSTALLED, Some("Iosevka Nerd Font")),
            None
        );
    }

    #[test]
    fn installed_selection_rejects_a_non_nerd_explicit_family() {
        assert_eq!(
            select_installed_nerd_font_family(INSTALLED, Some("Lato")),
            None
        );
    }

    #[test]
    fn installed_selection_returns_the_font_maps_canonical_spelling() {
        assert_eq!(
            select_installed_nerd_font_family(INSTALLED, Some("symbols nerd font mono")),
            Some("Symbols Nerd Font Mono".to_owned())
        );
        assert_eq!(
            select_installed_nerd_font_family(INSTALLED, None),
            Some("Symbols Nerd Font Mono".to_owned())
        );
    }

    #[test]
    fn installed_selection_does_not_allocate_from_an_iterators_size_hint() {
        struct MisleadingFamilies(bool);

        impl Iterator for MisleadingFamilies {
            type Item = &'static str;

            fn next(&mut self) -> Option<Self::Item> {
                (!std::mem::replace(&mut self.0, true)).then_some("Symbols Nerd Font Mono")
            }

            fn size_hint(&self) -> (usize, Option<usize>) {
                (usize::MAX, Some(usize::MAX))
            }
        }

        assert_eq!(
            select_installed_nerd_font_family(MisleadingFamilies(false), None),
            Some("Symbols Nerd Font Mono".to_owned())
        );
    }

    #[test]
    fn composition_keeps_the_ui_family_first_and_never_repeats_the_icon_family() {
        assert_eq!(
            compose_family_list("Lato", "Symbols Nerd Font Mono"),
            "Lato,Symbols Nerd Font Mono"
        );
        let composed = compose_family_list("Lato", "Symbols Nerd Font Mono");
        assert_eq!(
            compose_family_list(&composed, "symbols nerd font mono"),
            composed
        );
        assert_eq!(
            compose_family_list("", "Symbols Nerd Font"),
            "Symbols Nerd Font"
        );
        assert_eq!(compose_family_list("Lato", "  "), "Lato");
    }
}

#[cfg(all(test, feature = "render-cairo"))]
mod pango_tests {
    use pango::FontDescription;

    /// The composed description must keep the requested UI family first, so
    /// text still renders in it, and must never drop the size or weight the
    /// host asked for.
    #[test]
    fn the_ui_family_survives_icon_resolution() {
        let base = FontDescription::from_string("Lato Medium 12");
        let composed = super::with_icon_fallback(&base, Some("Symbols Nerd Font Mono"));
        assert_eq!(
            composed.family().unwrap_or_default().as_str(),
            "Lato,Symbols Nerd Font Mono"
        );
        assert_eq!(composed.size(), base.size());
        assert_eq!(composed.weight(), base.weight());
    }

    /// Enumerating the font map must not panic on a machine with no patched
    /// font: the description simply comes back unchanged.
    #[test]
    fn resolution_is_safe_on_any_desktop() {
        let base = FontDescription::from_string("Sans 10");
        let composed = super::with_icon_fallback(&base, None);
        let family = composed.family().unwrap_or_default();
        assert!(family.starts_with("Sans"), "unexpected family: {family}");
    }
}
