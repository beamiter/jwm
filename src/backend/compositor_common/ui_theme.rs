//! Backend-neutral look-and-feel for the compositor's own surfaces.
//!
//! Every panel JWM draws itself — the debug HUD, the modal system-UI card, the
//! toast stack and the volume/brightness OSD — used to carry Material tones
//! hardcoded at each draw site in both compositors. This module turns that into
//! one switchable palette so a second design language can exist beside Material
//! without either backend growing its own opinion.
//!
//! Three themes ship today, chosen by `appearance.ui_theme`:
//!
//! * [`UiTheme::Material`] — the original elevated surfaces: near-opaque dark
//!   cards separated from the desktop by a drop shadow.
//! * [`UiTheme::Glass`] — Apple's light frosted glass ("毛玻璃"), the material
//!   iOS and macOS use for folders, sheets and Control Center: a luminous sheet
//!   that *lifts* what is behind it, with continuous (squircle) corners, a
//!   beveled edge that refracts the backdrop, and a rim hairline all the way
//!   around. Depth comes from the optics, so the shadow is nearly absent.
//! * [`UiTheme::GlassDark`] — the same optics with a graphite veil, for people
//!   who want frosted panels without a light UI.
//!
//! Both glass themes carry a [`GlassParams`] block; Material's is `None`, which
//! is also how a renderer decides whether it needs a backdrop capture at all.

/// Which design language the compositor's own surfaces follow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum UiTheme {
    /// Material elevation: opaque surface plus drop shadow.
    #[default]
    Material,
    /// Apple's light frosted glass over a blurred backdrop.
    Glass,
    /// The same optics, tinted dark.
    GlassDark,
}

impl UiTheme {
    /// Parse `appearance.ui_theme`. Unknown values fall back to Material, which
    /// matches how the rest of the config treats an unrecognized choice: the
    /// validator reports it, the compositor still starts.
    pub(crate) fn from_config(name: &str) -> Self {
        match name.trim().to_ascii_lowercase().as_str() {
            "glass" | "glass-light" | "glass_light" | "acrylic" | "frosted" => Self::Glass,
            "glass-dark" | "glass_dark" => Self::GlassDark,
            _ => Self::Material,
        }
    }

    /// The tones, metrics and glass parameters for this theme.
    pub(crate) fn palette(self) -> &'static UiPalette {
        match self {
            Self::Material => &MATERIAL,
            Self::Glass => &GLASS,
            Self::GlassDark => &GLASS_DARK,
        }
    }

    /// True when surfaces sample a blurred copy of the scene, so the renderer
    /// must keep the blur FBO chain alive even with window blur switched off.
    pub(crate) fn needs_backdrop(self) -> bool {
        self.palette().glass.is_some()
    }
}

/// The active theme, read from the live config.
pub(crate) fn theme() -> UiTheme {
    UiTheme::from_config(crate::config::CONFIG.load().ui_theme())
}

/// The active theme's palette.
pub(crate) fn palette() -> &'static UiPalette {
    theme().palette()
}

/// Extra knobs the frosted-glass surface shader needs. All of them are uniform
/// inputs; a renderer that cannot supply a backdrop texture falls back to the
/// plain [`UiPalette`] fills.
///
/// The set models a *thick sheet of glass* rather than a translucent rectangle,
/// which is what separates Apple's material from a plain blur: the sheet has a
/// beveled edge that bends light ([`refraction`](Self::refraction)), a hairline
/// where that bevel meets the air ([`rim_intensity`](Self::rim_intensity)), and
/// continuous corners rather than circular ones
/// ([`corner_exponent`](Self::corner_exponent)).
#[derive(Debug, Clone, Copy)]
pub(crate) struct GlassParams {
    /// Kawase levels to run for the backdrop. Apple's material is blurred far
    /// past legibility of the content behind it, so this runs deeper than the
    /// per-window frost.
    pub(crate) blur_levels: u32,
    /// Chroma multiplier on the blurred backdrop: glass keeps the color of what
    /// it covers, slightly enriched, instead of graying it out.
    pub(crate) saturation: f32,
    /// Brightness multiplier on the blurred backdrop, applied before the tint.
    /// Above 1.0 for a light material — the sheet *lifts* what is behind it.
    pub(crate) luminance: f32,
    /// Superellipse exponent for the corners. 2.0 is an ordinary circular
    /// rounded rect; Apple's "continuous" corner is a squircle around 4–5,
    /// where the curvature ramps in instead of starting abruptly at the
    /// tangent point. This is the single most recognizable difference in
    /// silhouette between an Apple panel and a CSS `border-radius`.
    pub(crate) corner_exponent: f32,
    /// Width in pixels of the beveled band inside the edge, over which the
    /// lensing, inner glow and contact shade ramp up.
    pub(crate) bevel_width: f32,
    /// How far, in pixels, the bevel drags the backdrop outward. This is the
    /// refraction of a thick edge: content just outside the panel is squeezed
    /// into the rim, so the glass reads as having depth rather than being a
    /// decal.
    pub(crate) refraction: f32,
    /// Width in pixels of the specular hairline at the very edge.
    pub(crate) rim_width: f32,
    /// Strength of that hairline.
    pub(crate) rim_intensity: f32,
    /// Color of the rim. A faint cyan reads as glass; pure white reads as a
    /// plain stroke.
    pub(crate) rim_tint: [f32; 3],
    /// Broad diagonal sheen across the face, brightest at the top-left.
    pub(crate) sheen: f32,
    /// Strength of the contact shade along the bottom edge.
    pub(crate) edge_shade: f32,
    /// Amplitude of the dither grain that keeps wide blurred gradients from
    /// banding on 8-bit outputs.
    pub(crate) grain: f32,
}

/// Every tone and metric the self-drawn surfaces use, in one switchable set.
///
/// Surface colors are straight (non-premultiplied) RGBA; the renderers fold in
/// premultiplication. Under [`UiTheme::Glass`] the `card`/`panel`/`toast`/`osd`
/// entries are read as *tint over the blurred backdrop*: RGB is the tint hue,
/// alpha is how much of it covers the backdrop.
#[derive(Debug, Clone, Copy)]
pub(crate) struct UiPalette {
    /// `Some` for frosted themes; `None` means flat fills only.
    pub(crate) glass: Option<GlassParams>,

    // --- Surfaces ---
    /// Debug HUD card.
    pub(crate) card: [f32; 4],
    /// Modal system-UI panel (launcher, keybinding viewer, lock card).
    pub(crate) panel: [f32; 4],
    /// Toast card.
    pub(crate) toast: [f32; 4],
    /// OSD pill.
    pub(crate) osd: [f32; 4],
    /// Raised fill for the HUD state chip.
    pub(crate) chip: [f32; 4],
    /// Search/query field inside the modal panel.
    pub(crate) field: [f32; 4],
    /// Unfilled part of a meter or slider.
    pub(crate) track: [f32; 4],
    /// Filled part of the OSD slider, before the accent tint is applied.
    pub(crate) slider_track: [f32; 4],
    /// Full-screen dim behind the modal panel.
    pub(crate) scrim: [f32; 4],
    /// Opaque backdrop the lock card sits on.
    pub(crate) lock_backdrop: [f32; 4],
    /// Drop shadow under every card.
    pub(crate) shadow: [f32; 4],
    /// Multiplier on each surface's shadow spread. Glass leans on the blur for
    /// separation, so its shadow is wider and far softer.
    pub(crate) shadow_spread_scale: f32,
    /// Alpha multiplier on the accent ring around the HUD and the smaller cards.
    pub(crate) ring_alpha: f32,
    /// Alpha multiplier on the modal panel's ring, which is drawn heavier than
    /// the HUD's so a launcher reads as the frontmost thing on screen.
    pub(crate) panel_ring_alpha: f32,
    /// Ring thickness in pixels.
    pub(crate) ring_width: f32,
    /// Alpha of the selection pill under the highlighted list row.
    pub(crate) selection_alpha: f32,

    // --- Text tones, brightest first ---
    pub(crate) title_ink: [u8; 4],
    pub(crate) chip_ink: [u8; 4],
    pub(crate) label_ink: [u8; 4],
    pub(crate) value_ink: [u8; 4],
    /// Modal panel title.
    pub(crate) panel_title_ink: [u8; 4],
    /// Query line inside the panel's search field.
    pub(crate) query_ink: [u8; 4],
    /// Panel list rows.
    pub(crate) item_ink: [u8; 4],
    /// Panel footer hint.
    pub(crate) hint_ink: [u8; 4],
    /// OSD icon + reading.
    pub(crate) osd_ink: [u8; 4],

    // --- Metrics ---
    /// Distance from the screen corner to the HUD card.
    pub(crate) margin: f32,
    pub(crate) card_radius: f32,
    pub(crate) chip_radius: f32,
    pub(crate) panel_radius: f32,
    pub(crate) toast_radius: f32,
    pub(crate) osd_radius: f32,
    /// Card padding.
    pub(crate) pad: f32,
    /// Vertical rhythm inside a card.
    pub(crate) gap: f32,
    /// Space between the HUD's label and value columns.
    pub(crate) gutter: f32,
    /// Height of the HUD frame-rate meter.
    pub(crate) meter_h: f32,
    /// Base shadow spread for the HUD card.
    pub(crate) shadow_spread: f32,
}

impl UiPalette {
    /// Shadow spread for a surface whose Material spread is `base`.
    pub(crate) fn spread(&self, base: f32) -> f32 {
        base * self.shadow_spread_scale
    }

    /// The card tint with its alpha scaled by a fade envelope.
    pub(crate) fn faded(color: [f32; 4], alpha: f32) -> [f32; 4] {
        [color[0], color[1], color[2], color[3] * alpha]
    }
}

/// Material: elevated opaque surfaces on the 8dp grid. The original look.
pub(crate) const MATERIAL: UiPalette = UiPalette {
    glass: None,

    card: [0.071, 0.082, 0.114, 0.94],
    panel: [0.071, 0.082, 0.114, 0.985],
    toast: [0.075, 0.086, 0.118, 0.97],
    osd: [0.075, 0.086, 0.118, 0.97],
    chip: [0.108, 0.122, 0.165, 1.0],
    field: [0.108, 0.122, 0.165, 1.0],
    track: [1.0, 1.0, 1.0, 0.10],
    slider_track: [0.22, 0.25, 0.33, 0.9],
    scrim: [0.012, 0.016, 0.028, 0.62],
    lock_backdrop: [0.016, 0.020, 0.032, 1.0],
    shadow: [0.0, 0.0, 0.0, 0.55],
    shadow_spread_scale: 1.0,
    ring_alpha: 0.55,
    panel_ring_alpha: 0.95,
    ring_width: 1.0,
    selection_alpha: 0.26,

    title_ink: [214, 228, 255, 255],
    chip_ink: [150, 162, 186, 255],
    label_ink: [136, 148, 172, 255],
    value_ink: [232, 238, 250, 255],
    panel_title_ink: [205, 224, 255, 255],
    query_ink: [238, 242, 252, 255],
    item_ink: [216, 224, 240, 255],
    hint_ink: [140, 150, 172, 255],
    osd_ink: [232, 238, 250, 255],

    margin: 24.0,
    card_radius: 16.0,
    chip_radius: 9.0,
    panel_radius: 18.0,
    toast_radius: 14.0,
    osd_radius: 18.0,
    pad: 18.0,
    gap: 12.0,
    gutter: 18.0,
    meter_h: 4.0,
    shadow_spread: 36.0,
};

/// Apple's light frosted glass: the sheet is mostly the blurred desktop behind
/// it, *lifted* rather than darkened, veiled with enough white to hold dark
/// text over any wallpaper, and finished with the three optical cues that make
/// it read as a physical pane — continuous corners, a refracting bevel and a
/// rim hairline. Radii are larger and paddings roomier, matching the
/// proportions of iOS sheets and macOS Control Center.
///
/// The white veil is deliberately heavy enough (≈0.5) that even over a black
/// desktop the surface lands around mid-gray, so the dark inks below never
/// drop under roughly 6:1. That floor is what makes a light material safe on
/// a window manager, where the content behind it is whatever the user opened.
pub(crate) const GLASS: UiPalette = UiPalette {
    glass: Some(GlassParams {
        blur_levels: 4,
        saturation: 1.20,
        // Above 1: the sheet is a light source, not a filter.
        luminance: 1.06,
        corner_exponent: 4.2,
        bevel_width: 16.0,
        refraction: 9.0,
        rim_width: 1.8,
        rim_intensity: 0.55,
        rim_tint: [0.86, 0.95, 1.0],
        sheen: 0.05,
        edge_shade: 0.05,
        grain: 0.014,
    }),

    // White veil over the blurred backdrop. The modal panel carries the most
    // text, so it takes the heaviest one.
    card: [1.0, 1.0, 1.0, 0.46],
    panel: [1.0, 1.0, 1.0, 0.54],
    toast: [1.0, 1.0, 1.0, 0.48],
    osd: [1.0, 1.0, 1.0, 0.48],
    // Raised controls go lighter still; recessed tracks go dark, the way
    // they do on a light material.
    chip: [1.0, 1.0, 1.0, 0.55],
    field: [1.0, 1.0, 1.0, 0.50],
    track: [0.0, 0.0, 0.0, 0.12],
    slider_track: [0.0, 0.0, 0.0, 0.14],
    // The optics already separate the panel, so the scrim only needs to keep
    // the desktop from competing for attention.
    scrim: [0.04, 0.05, 0.08, 0.28],
    lock_backdrop: [0.10, 0.11, 0.14, 1.0],
    // Wide and faint: an ambient occlusion pool rather than an elevation cue.
    shadow: [0.0, 0.0, 0.0, 0.26],
    shadow_spread_scale: 1.5,
    // No accent ring: the shader's own rim hairline already outlines the
    // sheet, and the ring program draws a *circular* rounded rect, which would
    // trace a visibly different silhouette than the squircle mask.
    ring_alpha: 0.0,
    panel_ring_alpha: 0.0,
    ring_width: 1.0,
    // An accent wash has to work harder to show on a light surface.
    selection_alpha: 0.42,

    title_ink: [22, 26, 34, 255],
    chip_ink: [70, 78, 94, 255],
    label_ink: [86, 94, 110, 255],
    value_ink: [16, 20, 28, 255],
    panel_title_ink: [20, 24, 32, 255],
    query_ink: [12, 16, 24, 255],
    item_ink: [26, 30, 40, 255],
    hint_ink: [96, 104, 120, 255],
    osd_ink: [16, 20, 28, 255],

    margin: 28.0,
    card_radius: 22.0,
    chip_radius: 11.0,
    panel_radius: 26.0,
    toast_radius: 20.0,
    osd_radius: 24.0,
    pad: 20.0,
    gap: 13.0,
    gutter: 20.0,
    meter_h: 5.0,
    shadow_spread: 40.0,
};

/// The same glass optics under a graphite veil — macOS's dark vibrancy rather
/// than iOS's light sheet. Geometry and metrics are shared with [`GLASS`]; only
/// the veil, the inks and the rim's warmth differ.
pub(crate) const GLASS_DARK: UiPalette = UiPalette {
    glass: Some(GlassParams {
        blur_levels: 4,
        saturation: 1.30,
        // Below 1: a dark sheet absorbs before it tints.
        luminance: 0.90,
        corner_exponent: 4.2,
        bevel_width: 16.0,
        refraction: 9.0,
        rim_width: 1.6,
        // A dark pane catches a brighter, cooler-white rim.
        rim_intensity: 0.42,
        rim_tint: [0.92, 0.97, 1.0],
        sheen: 0.035,
        edge_shade: 0.10,
        grain: 0.016,
    }),

    card: [0.10, 0.12, 0.16, 0.55],
    panel: [0.09, 0.11, 0.15, 0.62],
    toast: [0.10, 0.12, 0.16, 0.55],
    osd: [0.10, 0.12, 0.16, 0.55],
    chip: [1.0, 1.0, 1.0, 0.18],
    field: [1.0, 1.0, 1.0, 0.15],
    track: [1.0, 1.0, 1.0, 0.22],
    slider_track: [1.0, 1.0, 1.0, 0.24],
    scrim: [0.02, 0.03, 0.05, 0.34],
    lock_backdrop: [0.03, 0.04, 0.06, 1.0],
    shadow: [0.0, 0.0, 0.0, 0.34],
    shadow_spread_scale: 1.45,
    // As in [`GLASS`]: the rim hairline is the boundary, and a circular ring
    // would not follow the squircle.
    ring_alpha: 0.0,
    panel_ring_alpha: 0.0,
    ring_width: 1.0,
    selection_alpha: 0.34,

    title_ink: [244, 248, 255, 255],
    chip_ink: [206, 216, 234, 255],
    label_ink: [198, 208, 226, 255],
    value_ink: [250, 252, 255, 255],
    panel_title_ink: [246, 250, 255, 255],
    query_ink: [252, 253, 255, 255],
    item_ink: [238, 243, 252, 255],
    hint_ink: [196, 206, 224, 255],
    osd_ink: [250, 252, 255, 255],

    margin: 28.0,
    card_radius: 22.0,
    chip_radius: 11.0,
    panel_radius: 26.0,
    toast_radius: 20.0,
    osd_radius: 24.0,
    pad: 20.0,
    gap: 13.0,
    gutter: 20.0,
    meter_h: 5.0,
    shadow_spread: 40.0,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_names_parse_case_insensitively() {
        assert_eq!(UiTheme::from_config("glass"), UiTheme::Glass);
        assert_eq!(UiTheme::from_config("  Glass "), UiTheme::Glass);
        assert_eq!(UiTheme::from_config("Glass-Dark"), UiTheme::GlassDark);
        assert_eq!(UiTheme::from_config("MATERIAL"), UiTheme::Material);
        // Anything unrecognized keeps the historical look.
        assert_eq!(UiTheme::from_config("neumorphic"), UiTheme::Material);
        assert_eq!(UiTheme::from_config(""), UiTheme::Material);
    }

    #[test]
    fn only_glass_wants_a_backdrop_capture() {
        assert!(UiTheme::Glass.needs_backdrop());
        assert!(UiTheme::GlassDark.needs_backdrop());
        assert!(!UiTheme::Material.needs_backdrop());
    }

    #[test]
    fn glass_surfaces_stay_translucent_enough_to_see_through() {
        for theme in [UiTheme::Glass, UiTheme::GlassDark] {
            let glass = theme.palette();
            for tint in [glass.card, glass.panel, glass.toast, glass.osd] {
                assert!(
                    tint[3] < 0.8,
                    "{theme:?}: a tint at {} would hide the backdrop it is meant to show",
                    tint[3]
                );
            }
        }
        // Material is the opposite contract: the surface must hide what's under it.
        let material = UiTheme::Material.palette();
        assert!(material.card[3] > 0.9);
    }

    /// Relative luminance, the WCAG way, for an 8-bit ink.
    fn relative_luminance(ink: [u8; 4]) -> f32 {
        let channel = |c: u8| {
            let c = f32::from(c) / 255.0;
            if c <= 0.03928 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(ink[0]) + 0.7152 * channel(ink[1]) + 0.0722 * channel(ink[2])
    }

    /// The light material only works if its veil is heavy enough that even a
    /// black desktop lands well above the dark ink underneath it. Model the
    /// worst case — backdrop 0, so the surface is exactly the veil — and
    /// require the usual 4.5:1 body-text ratio.
    #[test]
    fn light_glass_holds_its_ink_over_the_darkest_possible_desktop() {
        let glass = UiTheme::Glass.palette();
        let veil = glass.panel;
        assert!(
            veil[0] > 0.9 && veil[1] > 0.9 && veil[2] > 0.9,
            "the light material's veil must be white, got {veil:?}"
        );
        // Worst case: mix(black, white, coverage) == coverage, in sRGB.
        let surface = relative_luminance([
            (veil[3] * 255.0) as u8,
            (veil[3] * 255.0) as u8,
            (veil[3] * 255.0) as u8,
            255,
        ]);
        let ink = relative_luminance(glass.item_ink);
        let contrast = (surface + 0.05) / (ink + 0.05);
        assert!(
            contrast >= 4.5,
            "light glass over black would leave body text at {contrast:.1}:1"
        );
    }

    /// Both glass variants must carry the optics that distinguish Apple's
    /// material from a plain backdrop blur.
    #[test]
    fn glass_variants_share_the_apple_optics() {
        for theme in [UiTheme::Glass, UiTheme::GlassDark] {
            let params = theme
                .palette()
                .glass
                .unwrap_or_else(|| panic!("{theme:?} must carry glass params"));
            assert!(
                params.corner_exponent > 2.0,
                "{theme:?}: a corner exponent of {} is an ordinary circular radius",
                params.corner_exponent
            );
            assert!(
                params.refraction > 0.0 && params.bevel_width > params.rim_width,
                "{theme:?}: the bevel must be wider than the hairline it ends in"
            );
            assert!(params.rim_intensity > 0.0);
        }
        // The light sheet lifts what is behind it; the dark one absorbs first.
        assert!(GLASS.glass.unwrap().luminance > 1.0);
        assert!(GLASS_DARK.glass.unwrap().luminance < 1.0);
    }

    #[test]
    fn shadow_spread_scales_with_the_theme() {
        assert!((MATERIAL.spread(32.0) - 32.0).abs() < 1e-6);
        assert!(GLASS.spread(32.0) > 32.0);
    }

    #[test]
    fn fade_envelope_only_touches_alpha() {
        let faded = UiPalette::faded([0.1, 0.2, 0.3, 0.8], 0.5);
        assert_eq!(&faded[..3], &[0.1, 0.2, 0.3]);
        assert!((faded[3] - 0.4).abs() < 1e-6);
    }
}
