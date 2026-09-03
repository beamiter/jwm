//! Backend-neutral look-and-feel for the compositor's own surfaces.
//!
//! Every panel JWM draws itself — the debug HUD, the modal system-UI card, the
//! toast stack and the volume/brightness OSD — used to carry Material tones
//! hardcoded at each draw site in both compositors. This module turns that into
//! one switchable palette so a second design language can exist beside Material
//! without either backend growing its own opinion.
//!
//! Seven themes ship today, chosen by `appearance.ui_theme`:
//!
//! * [`UiTheme::Glass`] (default) — Apple's light frosted glass ("毛玻璃"),
//!   the material iOS and macOS use for folders, sheets and Control Center: a
//!   luminous sheet that *lifts* what is behind it, with continuous (squircle)
//!   corners, a beveled edge that refracts the backdrop, and a rim hairline
//!   all the way around. Depth comes from the optics, so the shadow is nearly
//!   absent.
//! * [`UiTheme::GlassDark`] — the same optics with a graphite veil, for people
//!   who want frosted panels without a light UI.
//! * [`UiTheme::Aurora`] — the glass optics under a deep indigo veil with an
//!   aurora-teal rim and richer chroma: tinted glass rather than neutral.
//! * [`UiTheme::Material`] — the original elevated surfaces: near-opaque dark
//!   cards separated from the desktop by a drop shadow.
//! * [`UiTheme::Nord`] — flat cards in the Nord palette: Polar Night surfaces
//!   under Snow Storm inks.
//! * [`UiTheme::TokyoNight`] — flat cards in the Tokyo Night palette: a
//!   near-black indigo ground under periwinkle inks.
//! * [`UiTheme::Paper`] — a light flat material: warm off-white opaque cards
//!   with dark ink, for desktops where frost is unwanted but Material's dark
//!   cards are too heavy.
//!
//! The glass themes carry a [`GlassParams`] block; the flat themes' is `None`,
//! which is also how a renderer decides whether it needs a backdrop capture at
//! all.

/// Which design language the compositor's own surfaces follow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum UiTheme {
    /// Material elevation: opaque surface plus drop shadow.
    Material,
    /// Apple's light frosted glass over a blurred backdrop.
    #[default]
    Glass,
    /// The same optics, tinted dark.
    GlassDark,
    /// The glass optics under an indigo veil with an aurora-teal rim.
    Aurora,
    /// Flat Nord: Polar Night surfaces, Snow Storm inks.
    Nord,
    /// Flat Tokyo Night: indigo ground, periwinkle inks.
    TokyoNight,
    /// Flat light material: off-white cards, dark ink.
    Paper,
}

impl UiTheme {
    /// Parse `appearance.ui_theme`. Unknown values fall back to the default
    /// ([`UiTheme::Glass`]), which matches how the rest of the config treats
    /// an unrecognized choice: the validator reports it, the compositor still
    /// starts.
    pub(crate) fn from_config(name: &str) -> Self {
        match name.trim().to_ascii_lowercase().as_str() {
            "material" => Self::Material,
            "glass-dark" | "glass_dark" => Self::GlassDark,
            "aurora" | "glass-aurora" | "glass_aurora" => Self::Aurora,
            "nord" => Self::Nord,
            "tokyo-night" | "tokyo_night" | "tokyonight" => Self::TokyoNight,
            "paper" | "light" => Self::Paper,
            "glass" | "glass-light" | "glass_light" | "acrylic" | "frosted" => Self::Glass,
            _ => Self::default(),
        }
    }

    /// The tones, metrics and glass parameters for this theme.
    pub(crate) fn palette(self) -> &'static UiPalette {
        match self {
            Self::Material => &MATERIAL,
            Self::Glass => &GLASS,
            Self::GlassDark => &GLASS_DARK,
            Self::Aurora => &AURORA,
            Self::Nord => &NORD,
            Self::TokyoNight => &TOKYO_NIGHT,
            Self::Paper => &PAPER,
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

/// Strength of the tab bar's hover chip relative to the focused cell's: the
/// hovered cell takes the same chip fill and accent wash at this scale, so
/// the pointer's target reads as raised without competing with the focus
/// indicator. The palette carries no hover token of its own — only
/// [`UiPalette::selection_alpha`] — and one shared scale keeps the two
/// compositors' bars pixel-identical.
pub(crate) const TAB_HOVER_ALPHA_SCALE: f32 = 0.5;

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

    /// One of the palette's text colors as a fill color, so a surface drawn
    /// with the ink of the labels beside it stays in step with the theme.
    pub(crate) fn ink(color: [u8; 4], alpha: f32) -> [f32; 4] {
        [
            color[0] as f32 / 255.0,
            color[1] as f32 / 255.0,
            color[2] as f32 / 255.0,
            (color[3] as f32 / 255.0) * alpha,
        ]
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
    card_radius: 16.0,
    chip_radius: 9.0,
    panel_radius: 18.0,
    toast_radius: 14.0,
    osd_radius: 18.0,
    pad: 18.0,
    gap: 12.0,
    gutter: 18.0,
    meter_h: 4.0,
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
    // Dark enough to clear 3:1 against the veil over a black desktop, which
    // the old [96, 104, 120] missed at 1.6:1 — the least legible text on the
    // panel was the line naming the keys. Still well above the rows, so the
    // footer stays a footer.
    hint_ink: [52, 60, 76, 255],
    osd_ink: [16, 20, 28, 255],
    card_radius: 22.0,
    chip_radius: 11.0,
    panel_radius: 26.0,
    toast_radius: 20.0,
    osd_radius: 24.0,
    pad: 20.0,
    gap: 13.0,
    gutter: 20.0,
    meter_h: 5.0,
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
    card_radius: 22.0,
    chip_radius: 11.0,
    panel_radius: 26.0,
    toast_radius: 20.0,
    osd_radius: 24.0,
    pad: 20.0,
    gap: 13.0,
    gutter: 20.0,
    meter_h: 5.0,
};

/// Tinted glass: the same pane as [`GLASS_DARK`] but the veil is a deep
/// indigo and the rim catches an aurora teal, so the panels read as colored
/// glass rather than smoked glass. Saturation is pushed harder — a tinted
/// pane is allowed to enrich the desktop it shows — and the shadow picks up
/// a violet cast instead of pure black.
pub(crate) const AURORA: UiPalette = UiPalette {
    glass: Some(GlassParams {
        blur_levels: 4,
        saturation: 1.45,
        luminance: 0.94,
        corner_exponent: 4.2,
        bevel_width: 16.0,
        refraction: 9.0,
        rim_width: 1.6,
        rim_intensity: 0.48,
        // Aurora teal, not neutral white: the rim is where the tint shows.
        rim_tint: [0.62, 0.95, 0.90],
        sheen: 0.045,
        edge_shade: 0.10,
        grain: 0.016,
    }),

    card: [0.10, 0.08, 0.20, 0.55],
    panel: [0.09, 0.07, 0.19, 0.62],
    toast: [0.10, 0.08, 0.20, 0.55],
    osd: [0.10, 0.08, 0.20, 0.55],
    // Raised controls take a periwinkle wash rather than plain white.
    chip: [0.62, 0.70, 1.0, 0.20],
    field: [0.62, 0.70, 1.0, 0.16],
    track: [1.0, 1.0, 1.0, 0.20],
    slider_track: [0.62, 0.95, 0.90, 0.30],
    scrim: [0.02, 0.02, 0.06, 0.36],
    lock_backdrop: [0.04, 0.03, 0.09, 1.0],
    shadow: [0.02, 0.0, 0.08, 0.36],
    shadow_spread_scale: 1.45,
    // As in the other glass themes: the rim hairline is the boundary.
    ring_alpha: 0.0,
    panel_ring_alpha: 0.0,
    ring_width: 1.0,
    selection_alpha: 0.36,

    title_ink: [240, 240, 255, 255],
    chip_ink: [204, 208, 240, 255],
    label_ink: [196, 200, 234, 255],
    value_ink: [248, 248, 255, 255],
    panel_title_ink: [242, 242, 255, 255],
    query_ink: [250, 250, 255, 255],
    item_ink: [232, 234, 252, 255],
    hint_ink: [188, 192, 226, 255],
    osd_ink: [248, 248, 255, 255],
    card_radius: 22.0,
    chip_radius: 11.0,
    panel_radius: 26.0,
    toast_radius: 20.0,
    osd_radius: 24.0,
    pad: 20.0,
    gap: 13.0,
    gutter: 20.0,
    meter_h: 5.0,
};

/// Flat Nord: Polar Night surfaces (nord0/nord1) under Snow Storm inks
/// (nord4–nord6). Material's geometry and elevation model, retoned — the
/// drop shadow stays, the accent ring stays, only the temperature changes.
pub(crate) const NORD: UiPalette = UiPalette {
    glass: None,

    card: [0.180, 0.204, 0.251, 0.96],
    panel: [0.180, 0.204, 0.251, 0.985],
    toast: [0.188, 0.212, 0.259, 0.97],
    osd: [0.188, 0.212, 0.259, 0.97],
    chip: [0.231, 0.259, 0.322, 1.0],
    field: [0.231, 0.259, 0.322, 1.0],
    track: [1.0, 1.0, 1.0, 0.10],
    slider_track: [0.298, 0.337, 0.416, 0.9],
    scrim: [0.08, 0.09, 0.12, 0.62],
    lock_backdrop: [0.145, 0.161, 0.196, 1.0],
    // Polar Night is lighter than Material's ground, so the shadow works a
    // little less hard and carries a hint of the palette's blue.
    shadow: [0.01, 0.02, 0.05, 0.50],
    shadow_spread_scale: 1.0,
    ring_alpha: 0.55,
    panel_ring_alpha: 0.95,
    ring_width: 1.0,
    selection_alpha: 0.28,

    title_ink: [236, 239, 244, 255],
    chip_ink: [170, 182, 200, 255],
    label_ink: [160, 172, 192, 255],
    value_ink: [229, 233, 240, 255],
    panel_title_ink: [236, 239, 244, 255],
    query_ink: [236, 239, 244, 255],
    item_ink: [216, 222, 233, 255],
    hint_ink: [143, 157, 179, 255],
    osd_ink: [229, 233, 240, 255],
    card_radius: 16.0,
    chip_radius: 9.0,
    panel_radius: 18.0,
    toast_radius: 14.0,
    osd_radius: 18.0,
    pad: 18.0,
    gap: 12.0,
    gutter: 18.0,
    meter_h: 4.0,
};

/// Flat Tokyo Night: the editor theme's near-black indigo ground under its
/// periwinkle foreground. Darker than Nord, cooler than Material.
pub(crate) const TOKYO_NIGHT: UiPalette = UiPalette {
    glass: None,

    card: [0.102, 0.106, 0.149, 0.95],
    panel: [0.102, 0.106, 0.149, 0.985],
    toast: [0.110, 0.114, 0.157, 0.97],
    osd: [0.110, 0.114, 0.157, 0.97],
    chip: [0.161, 0.180, 0.259, 1.0],
    field: [0.161, 0.180, 0.259, 1.0],
    track: [1.0, 1.0, 1.0, 0.09],
    slider_track: [0.253, 0.278, 0.400, 0.9],
    scrim: [0.035, 0.037, 0.055, 0.64],
    lock_backdrop: [0.063, 0.065, 0.094, 1.0],
    shadow: [0.0, 0.0, 0.02, 0.58],
    shadow_spread_scale: 1.0,
    ring_alpha: 0.55,
    panel_ring_alpha: 0.95,
    ring_width: 1.0,
    selection_alpha: 0.26,

    title_ink: [192, 202, 245, 255],
    chip_ink: [154, 165, 206, 255],
    label_ink: [139, 148, 189, 255],
    value_ink: [205, 214, 250, 255],
    panel_title_ink: [192, 202, 245, 255],
    query_ink: [212, 220, 252, 255],
    item_ink: [169, 177, 214, 255],
    hint_ink: [108, 117, 160, 255],
    osd_ink: [205, 214, 250, 255],
    card_radius: 16.0,
    chip_radius: 9.0,
    panel_radius: 18.0,
    toast_radius: 14.0,
    osd_radius: 18.0,
    pad: 18.0,
    gap: 12.0,
    gutter: 18.0,
    meter_h: 4.0,
};

/// Flat light material: warm off-white opaque cards with dark ink. The one
/// theme for people who want a light UI without the blur chain the glass
/// themes keep alive. Shadows are soft and slightly warm — a hard black
/// elevation shadow looks punched-out on a light ground.
pub(crate) const PAPER: UiPalette = UiPalette {
    glass: None,

    card: [0.976, 0.973, 0.965, 0.97],
    panel: [0.984, 0.982, 0.976, 0.99],
    toast: [0.976, 0.973, 0.965, 0.97],
    osd: [0.976, 0.973, 0.965, 0.97],
    // Raised controls recess slightly instead of lightening: there is no
    // headroom above an off-white card.
    chip: [0.922, 0.918, 0.906, 1.0],
    field: [0.929, 0.925, 0.914, 1.0],
    track: [0.0, 0.0, 0.0, 0.10],
    slider_track: [0.0, 0.0, 0.0, 0.14],
    scrim: [0.20, 0.20, 0.22, 0.30],
    lock_backdrop: [0.906, 0.902, 0.890, 1.0],
    shadow: [0.15, 0.14, 0.12, 0.30],
    shadow_spread_scale: 1.2,
    ring_alpha: 0.45,
    panel_ring_alpha: 0.8,
    ring_width: 1.0,
    // An accent wash has to work harder on a light surface, as in [`GLASS`].
    selection_alpha: 0.32,

    title_ink: [28, 27, 24, 255],
    chip_ink: [92, 90, 84, 255],
    label_ink: [108, 106, 100, 255],
    value_ink: [22, 21, 18, 255],
    panel_title_ink: [26, 25, 22, 255],
    query_ink: [16, 15, 12, 255],
    item_ink: [36, 35, 30, 255],
    hint_ink: [122, 120, 112, 255],
    osd_ink: [22, 21, 18, 255],
    card_radius: 14.0,
    chip_radius: 8.0,
    panel_radius: 16.0,
    toast_radius: 12.0,
    osd_radius: 16.0,
    pad: 18.0,
    gap: 12.0,
    gutter: 18.0,
    meter_h: 4.0,
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
        assert_eq!(UiTheme::from_config("Aurora"), UiTheme::Aurora);
        assert_eq!(UiTheme::from_config("Nord"), UiTheme::Nord);
        assert_eq!(UiTheme::from_config("Tokyo-Night"), UiTheme::TokyoNight);
        assert_eq!(UiTheme::from_config("tokyonight"), UiTheme::TokyoNight);
        assert_eq!(UiTheme::from_config("Paper"), UiTheme::Paper);
        // Anything unrecognized falls back to the default look.
        assert_eq!(UiTheme::from_config("neumorphic"), UiTheme::Glass);
        assert_eq!(UiTheme::from_config(""), UiTheme::Glass);
    }

    #[test]
    fn only_glass_wants_a_backdrop_capture() {
        assert!(UiTheme::Glass.needs_backdrop());
        assert!(UiTheme::GlassDark.needs_backdrop());
        assert!(UiTheme::Aurora.needs_backdrop());
        assert!(!UiTheme::Material.needs_backdrop());
        assert!(!UiTheme::Nord.needs_backdrop());
        assert!(!UiTheme::TokyoNight.needs_backdrop());
        assert!(!UiTheme::Paper.needs_backdrop());
    }

    #[test]
    fn glass_surfaces_stay_translucent_enough_to_see_through() {
        for theme in [UiTheme::Glass, UiTheme::GlassDark, UiTheme::Aurora] {
            let glass = theme.palette();
            for tint in [glass.card, glass.panel, glass.toast, glass.osd] {
                assert!(
                    tint[3] < 0.8,
                    "{theme:?}: a tint at {} would hide the backdrop it is meant to show",
                    tint[3]
                );
            }
        }
        // The flat themes are the opposite contract: the surface must hide
        // what's under it.
        for theme in [
            UiTheme::Material,
            UiTheme::Nord,
            UiTheme::TokyoNight,
            UiTheme::Paper,
        ] {
            let flat = theme.palette();
            assert!(
                flat.glass.is_none(),
                "{theme:?} must not ask for a backdrop"
            );
            assert!(
                flat.card[3] > 0.9,
                "{theme:?}: a flat card at {} would show through",
                flat.card[3]
            );
        }
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

    /// Worst-case panel surface for a theme: its tint composited over a black
    /// desktop, which for straight alpha is just the tint scaled by coverage.
    /// A flat opaque theme's own colour is already that worst case.
    fn worst_case_panel(palette: &UiPalette) -> [u8; 4] {
        let panel = palette.panel;
        let channel = |c: f32| (c * panel[3] * 255.0).clamp(0.0, 255.0) as u8;
        [channel(panel[0]), channel(panel[1]), channel(panel[2]), 255]
    }

    fn contrast(a: [u8; 4], b: [u8; 4]) -> f32 {
        let (a, b) = (relative_luminance(a) + 0.05, relative_luminance(b) + 0.05);
        if a > b { a / b } else { b / a }
    }

    /// The footer hint is the one line on the panel that tells a first-time
    /// user which keys do anything, and it used to be the least legible thing
    /// on screen — 1.6:1 under the default theme, against 4.7:1 for the rows
    /// above it. It is deliberately quieter than body text, so it is held to
    /// WCAG's 3:1 floor for large and incidental text rather than the 4.5:1
    /// body ratio, but quieter is not the same as invisible.
    #[test]
    fn every_theme_keeps_its_footer_hint_readable() {
        for theme in [
            UiTheme::Glass,
            UiTheme::GlassDark,
            UiTheme::Aurora,
            UiTheme::Material,
            UiTheme::Nord,
            UiTheme::TokyoNight,
            UiTheme::Paper,
        ] {
            let palette = theme.palette();
            let surface = worst_case_panel(palette);
            let hint = contrast(surface, palette.hint_ink);
            assert!(hint >= 3.0, "{theme:?} draws its hint at {hint:.1}:1");

            // ... and still recessed relative to the rows it sits under, or it
            // stops reading as a footer.
            let item = contrast(surface, palette.item_ink);
            assert!(
                item > hint,
                "{theme:?} hint ({hint:.1}:1) is not quieter than its rows ({item:.1}:1)"
            );
        }
    }

    /// The typed query is the opposite case: it is what the user is looking at
    /// while they type, so it gets the body ratio.
    #[test]
    fn every_theme_keeps_its_query_line_readable() {
        for theme in [
            UiTheme::Glass,
            UiTheme::GlassDark,
            UiTheme::Aurora,
            UiTheme::Material,
            UiTheme::Nord,
            UiTheme::TokyoNight,
            UiTheme::Paper,
        ] {
            let palette = theme.palette();
            let ratio = contrast(worst_case_panel(palette), palette.query_ink);
            assert!(ratio >= 4.5, "{theme:?} draws its query at {ratio:.1}:1");
        }
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
        for theme in [UiTheme::Glass, UiTheme::GlassDark, UiTheme::Aurora] {
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
        // The light sheet lifts what is behind it; the dark ones absorb first.
        assert!(GLASS.glass.unwrap().luminance > 1.0);
        assert!(GLASS_DARK.glass.unwrap().luminance < 1.0);
        assert!(AURORA.glass.unwrap().luminance < 1.0);
    }

    /// The light flat theme carries the same obligation as the light glass:
    /// dark ink over its lightest surface must clear body-text contrast.
    #[test]
    fn paper_holds_its_ink() {
        let paper = UiTheme::Paper.palette();
        let surface = relative_luminance([
            (paper.panel[0] * 255.0) as u8,
            (paper.panel[1] * 255.0) as u8,
            (paper.panel[2] * 255.0) as u8,
            255,
        ]);
        let ink = relative_luminance(paper.item_ink);
        let contrast = (surface + 0.05) / (ink + 0.05);
        assert!(
            contrast >= 4.5,
            "paper would leave body text at {contrast:.1}:1"
        );
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
