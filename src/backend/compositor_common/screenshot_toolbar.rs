//! The screenshot toolbar: its model, its geometry and its iconography.
//!
//! Once a region is committed the capture stops being a drag and becomes an
//! editor, and an editor needs a visible set of tools rather than a keymap you
//! have to already know. This module is that toolbar — a rounded track of round
//! buttons that floats just outside the selection, in the manner of Flameshot.
//!
//! It follows the same split that [`window_tabs`](super::window_tabs) uses, and
//! for the same reason: the window manager builds the model and hit-tests
//! clicks against it, the two compositors paint it, and *all three* derive every
//! rectangle from the functions here. A button therefore does what it looks
//! like it does, because the rectangle the click resolves against is literally
//! the rectangle that was painted.
//!
//! The toolbar carries its own `button_size`. Sizing is a fitting problem —
//! twenty buttons do not fit beside a selection on a small screen — and solving
//! it once on the window manager side, then shipping the answer, is what keeps
//! the painted strip and the hit-tested strip from disagreeing whenever the
//! monitor is narrow.
//!
//! Icons are rasterised here too, from signed distance fields, rather than
//! taken from a font. An icon font would make every glyph a bet on what the
//! user happens to have installed; a distance field is exact at any size, is
//! anti-aliased for free, and is the same handful of shapes on both backends.

/// A rectangle in screen pixels: `[x, y, w, h]`.
pub type Rect = [f32; 4];

/// What a button shows. Both the compositors' painters and the window
/// manager's layout switch on this, so a button that reads as text is a button
/// that is *measured* as text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ToolbarIcon {
    /// Freehand drawing.
    Pencil,
    /// Straight line.
    Line,
    /// Straight line with a head.
    Arrow,
    /// Hollow rectangle.
    RectOutline,
    /// Solid rectangle.
    RectFilled,
    /// Hollow ellipse.
    Ellipse,
    /// Translucent highlighter.
    Marker,
    /// Typed label.
    Text,
    /// Auto-incrementing numbered bubble.
    Counter,
    /// Mosaic over a region.
    Pixelate,
    /// Inverted colors over a region.
    Invert,
    /// Thinner stroke.
    Thinner,
    /// Thicker stroke.
    Thicker,
    /// The current ink, drawn in that ink.
    Color,
    /// Drop the last annotation.
    Undo,
    /// Put back the last dropped annotation.
    Redo,
    /// Finish, to the clipboard.
    Copy,
    /// Finish, to a file.
    Save,
    /// Abandon the capture.
    Close,
}

/// What one button draws inside itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ButtonFace {
    Icon(ToolbarIcon),
    /// A short readout — the selection's pixel size — drawn as text. Wider
    /// than an icon button, which is why measurement goes through
    /// [`face_units`] rather than assuming every button is square.
    Label(String),
}

/// One button of the toolbar.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolbarButton {
    pub face: ButtonFace,
    /// This button's tool is the current one; painted as a filled chip.
    pub active: bool,
    /// The pointer is over this button.
    pub hovered: bool,
    /// A disabled button is dimmed and hit-tests as a miss, so an undo with
    /// nothing to undo cannot swallow a click that was meant for the canvas.
    pub enabled: bool,
    /// Ink for [`ToolbarIcon::Color`]; ignored by every other face. Carrying
    /// the swatch on the button rather than on the toolbar keeps the
    /// compositors from having to know what the current annotation color is.
    pub tint: Option<[u8; 4]>,
}

impl ToolbarButton {
    /// An enabled, unselected icon button — what most buttons are.
    #[must_use]
    pub fn icon(icon: ToolbarIcon) -> Self {
        Self {
            face: ButtonFace::Icon(icon),
            active: false,
            hovered: false,
            enabled: true,
            tint: None,
        }
    }

    /// A read-only text cell. Never active, never a click target.
    #[must_use]
    pub fn label(text: impl Into<String>) -> Self {
        Self {
            face: ButtonFace::Label(text.into()),
            active: false,
            hovered: false,
            enabled: false,
            tint: None,
        }
    }

    #[must_use]
    pub fn selected(mut self, active: bool) -> Self {
        self.active = active;
        self
    }

    #[must_use]
    pub fn available(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    #[must_use]
    pub fn tinted(mut self, tint: [u8; 4]) -> Self {
        self.tint = Some(tint);
        self
    }
}

/// The whole strip: where it goes, how big its buttons are, and what is in it.
///
/// `button_size` travels with the model because it is the output of a fit that
/// only the window manager can perform — it is the one side that knows the
/// monitor. Every geometry function here takes it rather than re-deriving it.
#[derive(Clone, Debug, PartialEq)]
pub struct ScreenshotToolbar {
    /// The painted track, in screen pixels.
    pub bar: Rect,
    /// Diameter of a round button, already fitted to the monitor.
    pub button_size: f32,
    pub buttons: Vec<ToolbarButton>,
}

// ---------------------------------------------------------------------------
// Metrics
// ---------------------------------------------------------------------------

/// Diameter of a button at full size.
pub const BUTTON_SIZE: f32 = 30.0;
/// Below this the icons stop being legible, so a strip that still would not fit
/// is allowed to overhang instead of shrinking into mush.
pub const MIN_BUTTON_SIZE: f32 = 15.0;
/// Gap between neighbouring buttons, at full size.
pub const BUTTON_GAP: f32 = 5.0;
/// Track padding around the buttons, at full size.
pub const PAD_X: f32 = 8.0;
pub const PAD_Y: f32 = 6.0;
/// Air between the selection's edge and the track.
pub const SELECTION_GAP: f32 = 11.0;
/// Air the track keeps from the edge of the screen.
pub const SCREEN_MARGIN: f32 = 6.0;
/// How many buttons wide a text cell is.
pub const LABEL_UNITS: f32 = 2.1;
/// Fraction of a button the icon inside it occupies.
pub const ICON_SCALE: f32 = 0.54;
/// Point size for a label, as a fraction of the button.
pub const LABEL_FONT_SCALE: f32 = 0.34;

/// Width of one face in *button units*, so every metric scales together when
/// the strip is fitted to a narrow monitor.
#[must_use]
pub fn face_units(face: &ButtonFace) -> f32 {
    match face {
        ButtonFace::Icon(_) => 1.0,
        ButtonFace::Label(_) => LABEL_UNITS,
    }
}

/// Total width of the track in button units, padding and gaps included.
#[must_use]
pub fn track_units(buttons: &[ToolbarButton]) -> f32 {
    if buttons.is_empty() {
        return 0.0;
    }
    let faces: f32 = buttons.iter().map(|b| face_units(&b.face)).sum();
    let gaps = BUTTON_GAP / BUTTON_SIZE * (buttons.len() - 1) as f32;
    faces + gaps + 2.0 * PAD_X / BUTTON_SIZE
}

/// Height of the track in button units.
#[must_use]
pub fn track_height_units() -> f32 {
    1.0 + 2.0 * PAD_Y / BUTTON_SIZE
}

/// The largest button size whose track still fits `max_width`, never above the
/// nominal size and never below the legibility floor. A strip that cannot fit
/// even at the floor keeps the floor and is clamped into the screen by
/// [`place`], which is the least-bad of the two ways to lose.
#[must_use]
pub fn fit_button_size(buttons: &[ToolbarButton], max_width: f32) -> f32 {
    let units = track_units(buttons);
    if !max_width.is_finite() || max_width <= 0.0 || units <= 0.0 {
        return BUTTON_SIZE;
    }
    (max_width / units).clamp(MIN_BUTTON_SIZE, BUTTON_SIZE)
}

/// The track's pixel size for a given button size.
#[must_use]
pub fn track_extent(buttons: &[ToolbarButton], button_size: f32) -> (f32, f32) {
    (
        track_units(buttons) * button_size,
        track_height_units() * button_size,
    )
}

/// Where the track goes: centred on the selection, below it by preference,
/// above it when there is no room below, and tucked inside its bottom edge when
/// there is room in neither — always clamped into `screen`.
///
/// Preferring *below* matches where the eye already is after a downward drag,
/// and matches Flameshot, so the muscle memory transfers.
#[must_use]
pub fn place(selection: Rect, screen: Rect, extent: (f32, f32)) -> Rect {
    let (w, h) = extent;
    let [sx, sy, sw, sh] = selection;
    let [scx, scy, scw, sch] = screen;

    // A screen narrower than the strip makes the clamp inverted; keeping the
    // low bound in that case pins the strip to the left edge rather than
    // pushing it off the right one.
    let min_x = scx + SCREEN_MARGIN;
    let max_x = (scx + scw - SCREEN_MARGIN - w).max(min_x);
    let x = (sx + sw * 0.5 - w * 0.5).clamp(min_x, max_x);

    let min_y = scy + SCREEN_MARGIN;
    let max_y = (scy + sch - SCREEN_MARGIN - h).max(min_y);
    let below = sy + sh + SELECTION_GAP;
    let above = sy - SELECTION_GAP - h;
    let y = if below <= max_y {
        below
    } else if above >= min_y {
        above
    } else {
        // Neither side has room: ride the selection's bottom edge from the
        // inside, where the strip at least stays attached to what it edits.
        (sy + sh - h - SELECTION_GAP).clamp(min_y, max_y)
    };

    [x.round(), y.round(), w, h]
}

/// The *slot* for `index`: its share of the track, split with its neighbours
/// down the middle of the gap between them. Slots tile the track edge to edge,
/// so [`button_at`] leaves no dead pixels between two buttons.
#[must_use]
pub fn slot_rect(
    bar: Rect,
    buttons: &[ToolbarButton],
    button_size: f32,
    index: usize,
) -> Option<Rect> {
    if index >= buttons.len() || !is_drawable(bar) || !is_positive(button_size) {
        return None;
    }
    let [bx, by, bw, bh] = bar;
    let gap = BUTTON_GAP / BUTTON_SIZE * button_size;
    let pad = PAD_X / BUTTON_SIZE * button_size;

    let mut cursor = bx + pad;
    for button in &buttons[..index] {
        cursor += face_units(&button.face) * button_size + gap;
    }
    let width = face_units(&buttons[index].face) * button_size;

    // Outer slots absorb the padding, inner edges take half a gap, so the row
    // of slots covers the whole track without overlapping.
    let left = if index == 0 { pad } else { gap * 0.5 };
    let right = if index + 1 == buttons.len() {
        pad
    } else {
        gap * 0.5
    };
    let slot = [cursor - left, by, width + left + right, bh];
    // A slot that ran past the track (a strip wider than its own bar) is
    // clipped rather than allowed to hit-test outside the paint.
    let clipped_w = slot[2].min(bx + bw - slot[0]);
    is_drawable([slot[0], slot[1], clipped_w, slot[3]])
        .then_some([slot[0], slot[1], clipped_w, slot[3]])
}

/// The *painted* button for `index`: a circle-sized square centred in its slot.
#[must_use]
pub fn button_rect(
    bar: Rect,
    buttons: &[ToolbarButton],
    button_size: f32,
    index: usize,
) -> Option<Rect> {
    if index >= buttons.len() || !is_drawable(bar) || !is_positive(button_size) {
        return None;
    }
    let [bx, by, _, bh] = bar;
    let gap = BUTTON_GAP / BUTTON_SIZE * button_size;
    let pad = PAD_X / BUTTON_SIZE * button_size;

    let mut cursor = bx + pad;
    for button in &buttons[..index] {
        cursor += face_units(&button.face) * button_size + gap;
    }
    let width = face_units(&buttons[index].face) * button_size;
    let height = button_size.min(bh);
    let rect = [cursor, by + (bh - height) * 0.5, width, height];
    is_drawable(rect).then_some(rect)
}

/// Which button `(px, py)` lands on, if any. Disabled buttons are misses: they
/// are decoration, and a click that falls on one should reach whatever is
/// behind the toolbar rather than being eaten by a no-op.
#[must_use]
pub fn button_at(
    bar: Rect,
    buttons: &[ToolbarButton],
    button_size: f32,
    px: f32,
    py: f32,
) -> Option<usize> {
    if !contains(bar, px, py) {
        return None;
    }
    (0..buttons.len()).find(|&index| {
        buttons[index].enabled
            && slot_rect(bar, buttons, button_size, index).is_some_and(|s| contains(s, px, py))
    })
}

/// Whether `(px, py)` is anywhere on the track — used to decide that a press
/// belongs to the toolbar and must not start an annotation, even when it landed
/// in the padding between two buttons.
#[must_use]
pub fn hits_toolbar(bar: Rect, px: f32, py: f32) -> bool {
    contains(bar, px, py)
}

/// Corner radius that turns a rectangle this tall into a pill.
#[must_use]
pub fn pill_radius(height: f32) -> f32 {
    if height.is_finite() {
        (height * 0.5).max(0.0)
    } else {
        0.0
    }
}

/// Side of the icon drawn inside a button of this size.
#[must_use]
pub fn icon_extent(button_size: f32) -> u32 {
    if !button_size.is_finite() {
        return 0;
    }
    (button_size * ICON_SCALE).round().max(1.0) as u32
}

/// Point size for a [`ButtonFace::Label`] in a button of this size.
#[must_use]
pub fn label_font_size(button_size: f32) -> f32 {
    if button_size.is_finite() {
        (button_size * LABEL_FONT_SCALE).clamp(7.0, 14.0)
    } else {
        BUTTON_SIZE * LABEL_FONT_SCALE
    }
}

/// A finite, strictly positive size. Spelled out rather than written as
/// `!(size > 0.0)` so the NaN case — which must also be rejected — is visible.
fn is_positive(value: f32) -> bool {
    value.is_finite() && value > 0.0
}

fn is_drawable(rect: Rect) -> bool {
    let [x, y, w, h] = rect;
    x.is_finite() && y.is_finite() && w.is_finite() && h.is_finite() && w > 0.0 && h > 0.0
}

fn contains(rect: Rect, px: f32, py: f32) -> bool {
    let [x, y, w, h] = rect;
    is_drawable(rect) && px >= x && px <= x + w && py >= y && py <= y + h
}

// ---------------------------------------------------------------------------
// Icon rasterization
// ---------------------------------------------------------------------------

/// One primitive of an icon, in a unit square with `y` growing downwards.
///
/// Every icon is a union of these, evaluated as a signed distance field. Union
/// is `min`, which is why nothing here needs a painter's algorithm: the whole
/// glyph is one distance, and one distance anti-aliases in one step.
#[derive(Clone, Copy, Debug)]
enum Shape {
    /// A round-capped stroke from `a` to `b` of half-width `r`.
    Capsule { a: [f32; 2], b: [f32; 2], r: f32 },
    /// A circle outline of radius `rad`, half-width `r`.
    Ring { c: [f32; 2], rad: f32, r: f32 },
    /// A solid disc.
    Disc { c: [f32; 2], rad: f32 },
    /// The right half of a solid disc — the "invert" glyph's filled side.
    HalfDisc { c: [f32; 2], rad: f32 },
    /// A rectangle outline of half-width `r`.
    Frame {
        min: [f32; 2],
        max: [f32; 2],
        r: f32,
    },
    /// A solid rectangle.
    Fill { min: [f32; 2], max: [f32; 2] },
}

impl Shape {
    /// Widen any stroke thinner than `min_r` (in unit coordinates) up to it.
    ///
    /// Strokes are specified as a fraction of the icon, so at the small end of
    /// the size range a hairline lands between two pixel centres and rasterises
    /// to a smear of half-lit pixels — the "thinner stroke" button in
    /// particular disappeared entirely at the 15px floor. Holding every stroke
    /// to a little under a pixel of half-width keeps each glyph solid at every
    /// size the toolbar is ever drawn at, while leaving the *relative* weights
    /// (thin bar versus fat bar) intact.
    fn with_min_stroke(self, min_r: f32) -> Self {
        match self {
            Self::Capsule { a, b, r } => Self::Capsule {
                a,
                b,
                r: r.max(min_r),
            },
            Self::Ring { c, rad, r } => Self::Ring {
                c,
                rad,
                r: r.max(min_r),
            },
            Self::Frame { min, max, r } => Self::Frame {
                min,
                max,
                r: r.max(min_r),
            },
            other => other,
        }
    }

    /// Signed distance from `p`, negative inside.
    fn distance(&self, p: [f32; 2]) -> f32 {
        match *self {
            Self::Capsule { a, b, r } => sd_segment(p, a, b) - r,
            Self::Ring { c, rad, r } => (length(sub(p, c)) - rad).abs() - r,
            Self::Disc { c, rad } => length(sub(p, c)) - rad,
            Self::HalfDisc { c, rad } => (length(sub(p, c)) - rad).max(c[0] - p[0]),
            Self::Frame { min, max, r } => sd_box(p, min, max).abs() - r,
            Self::Fill { min, max } => sd_box(p, min, max),
        }
    }
}

fn sub(a: [f32; 2], b: [f32; 2]) -> [f32; 2] {
    [a[0] - b[0], a[1] - b[1]]
}

fn length(v: [f32; 2]) -> f32 {
    (v[0] * v[0] + v[1] * v[1]).sqrt()
}

fn sd_segment(p: [f32; 2], a: [f32; 2], b: [f32; 2]) -> f32 {
    let pa = sub(p, a);
    let ba = sub(b, a);
    let denom = ba[0] * ba[0] + ba[1] * ba[1];
    let t = if denom > f32::EPSILON {
        ((pa[0] * ba[0] + pa[1] * ba[1]) / denom).clamp(0.0, 1.0)
    } else {
        0.0
    };
    length([pa[0] - ba[0] * t, pa[1] - ba[1] * t])
}

fn sd_box(p: [f32; 2], min: [f32; 2], max: [f32; 2]) -> f32 {
    let c = [(min[0] + max[0]) * 0.5, (min[1] + max[1]) * 0.5];
    let half = [(max[0] - min[0]) * 0.5, (max[1] - min[1]) * 0.5];
    let q = [(p[0] - c[0]).abs() - half[0], (p[1] - c[1]).abs() - half[1]];
    let outside = length([q[0].max(0.0), q[1].max(0.0)]);
    outside + q[0].max(q[1]).min(0.0)
}

/// Half-width of an ordinary icon stroke, in unit coordinates.
const STROKE: f32 = 0.048;

/// Half-width, in *pixels*, below which a stroke stops rasterising to anything
/// solid. See [`Shape::with_min_stroke`].
const MIN_STROKE_PX: f32 = 0.85;

/// Sample an arc into capsules. `from`/`to` are radians in the icon's
/// y-down frame, where `-PI/2` points at the top of the square.
fn arc(out: &mut Vec<Shape>, c: [f32; 2], rad: f32, from: f32, to: f32, r: f32) {
    const STEPS: usize = 12;
    let mut previous = [c[0] + rad * from.cos(), c[1] + rad * from.sin()];
    for i in 1..=STEPS {
        let t = from + (to - from) * i as f32 / STEPS as f32;
        let next = [c[0] + rad * t.cos(), c[1] + rad * t.sin()];
        out.push(Shape::Capsule {
            a: previous,
            b: next,
            r,
        });
        previous = next;
    }
}

fn shapes_for(icon: ToolbarIcon) -> Vec<Shape> {
    use std::f32::consts::PI;
    let s = STROKE;
    match icon {
        ToolbarIcon::Pencil => vec![
            // Barrel, then the exposed nib at its lower-left end.
            Shape::Capsule {
                a: [0.34, 0.66],
                b: [0.74, 0.26],
                r: 0.085,
            },
            Shape::Capsule {
                a: [0.22, 0.78],
                b: [0.33, 0.67],
                r: 0.035,
            },
        ],
        ToolbarIcon::Line => vec![Shape::Capsule {
            a: [0.22, 0.78],
            b: [0.78, 0.22],
            r: s,
        }],
        ToolbarIcon::Arrow => vec![
            Shape::Capsule {
                a: [0.78, 0.22],
                b: [0.26, 0.74],
                r: s,
            },
            Shape::Capsule {
                a: [0.26, 0.74],
                b: [0.26, 0.50],
                r: s,
            },
            Shape::Capsule {
                a: [0.26, 0.74],
                b: [0.50, 0.74],
                r: s,
            },
        ],
        ToolbarIcon::RectOutline => vec![Shape::Frame {
            min: [0.22, 0.28],
            max: [0.78, 0.72],
            r: s,
        }],
        ToolbarIcon::RectFilled => vec![Shape::Fill {
            min: [0.22, 0.28],
            max: [0.78, 0.72],
        }],
        ToolbarIcon::Ellipse => vec![Shape::Ring {
            c: [0.5, 0.5],
            rad: 0.28,
            r: s,
        }],
        ToolbarIcon::Marker => vec![
            // A fat barrel and a square chisel nib, so it never reads as the
            // pencil next to it.
            Shape::Capsule {
                a: [0.38, 0.62],
                b: [0.74, 0.26],
                r: 0.115,
            },
            Shape::Fill {
                min: [0.17, 0.66],
                max: [0.36, 0.85],
            },
        ],
        ToolbarIcon::Text => vec![
            Shape::Capsule {
                a: [0.24, 0.27],
                b: [0.76, 0.27],
                r: s,
            },
            Shape::Capsule {
                a: [0.50, 0.27],
                b: [0.50, 0.76],
                r: s,
            },
        ],
        ToolbarIcon::Counter => vec![
            Shape::Ring {
                c: [0.5, 0.5],
                rad: 0.31,
                r: s,
            },
            // A numeral one inside the bubble.
            Shape::Capsule {
                a: [0.52, 0.32],
                b: [0.52, 0.68],
                r: 0.04,
            },
            Shape::Capsule {
                a: [0.40, 0.42],
                b: [0.52, 0.32],
                r: 0.035,
            },
        ],
        ToolbarIcon::Pixelate => {
            // A 4x4 board of tiles: the one icon that has to read as
            // "resolution thrown away".
            let mut shapes = Vec::with_capacity(16);
            let cell = 0.145;
            let gap = 0.035;
            let start = 0.5 - (4.0 * cell + 3.0 * gap) * 0.5;
            for row in 0..4 {
                for col in 0..4 {
                    if (row + col) % 2 == 1 {
                        continue;
                    }
                    let x = start + col as f32 * (cell + gap);
                    let y = start + row as f32 * (cell + gap);
                    shapes.push(Shape::Fill {
                        min: [x, y],
                        max: [x + cell, y + cell],
                    });
                }
            }
            // The empty squares still need a board to sit on.
            for row in 0..4 {
                for col in 0..4 {
                    if (row + col) % 2 == 0 {
                        continue;
                    }
                    let x = start + col as f32 * (cell + gap);
                    let y = start + row as f32 * (cell + gap);
                    shapes.push(Shape::Frame {
                        min: [x + 0.012, y + 0.012],
                        max: [x + cell - 0.012, y + cell - 0.012],
                        r: 0.012,
                    });
                }
            }
            shapes
        }
        ToolbarIcon::Invert => vec![
            Shape::Ring {
                c: [0.5, 0.5],
                rad: 0.30,
                r: s,
            },
            Shape::HalfDisc {
                c: [0.5, 0.5],
                rad: 0.30,
            },
        ],
        ToolbarIcon::Thinner => vec![Shape::Capsule {
            a: [0.24, 0.5],
            b: [0.76, 0.5],
            r: 0.028,
        }],
        ToolbarIcon::Thicker => vec![Shape::Capsule {
            a: [0.24, 0.5],
            b: [0.76, 0.5],
            r: 0.105,
        }],
        ToolbarIcon::Color => vec![Shape::Disc {
            c: [0.5, 0.5],
            rad: 0.30,
        }],
        ToolbarIcon::Undo => {
            // An arc over the top, from right to left, with the head on the
            // left end pointing down — the direction of travel.
            let mut shapes = Vec::new();
            arc(&mut shapes, [0.5, 0.58], 0.26, PI, 2.0 * PI, s);
            shapes.push(Shape::Capsule {
                a: [0.24, 0.58],
                b: [0.15, 0.44],
                r: s,
            });
            shapes.push(Shape::Capsule {
                a: [0.24, 0.58],
                b: [0.36, 0.47],
                r: s,
            });
            shapes
        }
        ToolbarIcon::Redo => {
            let mut shapes = Vec::new();
            arc(&mut shapes, [0.5, 0.58], 0.26, PI, 2.0 * PI, s);
            shapes.push(Shape::Capsule {
                a: [0.76, 0.58],
                b: [0.85, 0.44],
                r: s,
            });
            shapes.push(Shape::Capsule {
                a: [0.76, 0.58],
                b: [0.64, 0.47],
                r: s,
            });
            shapes
        }
        ToolbarIcon::Copy => vec![
            Shape::Frame {
                min: [0.19, 0.19],
                max: [0.62, 0.62],
                r: s * 0.85,
            },
            Shape::Frame {
                min: [0.38, 0.38],
                max: [0.81, 0.81],
                r: s * 0.85,
            },
        ],
        ToolbarIcon::Save => vec![
            // A floppy: body, shutter, label.
            Shape::Frame {
                min: [0.20, 0.20],
                max: [0.80, 0.80],
                r: s * 0.85,
            },
            Shape::Fill {
                min: [0.34, 0.20],
                max: [0.66, 0.40],
            },
            Shape::Frame {
                min: [0.32, 0.56],
                max: [0.68, 0.80],
                r: s * 0.7,
            },
        ],
        ToolbarIcon::Close => vec![
            Shape::Capsule {
                a: [0.28, 0.28],
                b: [0.72, 0.72],
                r: s,
            },
            Shape::Capsule {
                a: [0.72, 0.28],
                b: [0.28, 0.72],
                r: s,
            },
        ],
    }
}

/// Rasterise `icon` into a straight-alpha RGBA square of `px` on a side,
/// tinted `ink`.
///
/// The coverage comes from the distance field directly — a pixel one unit
/// inside the shape is opaque, one unit outside is clear, and the band between
/// is the anti-aliasing. That is why the icons stay crisp at the 15px floor and
/// at the 30px nominal size without a single hand-tuned bitmap.
#[must_use]
pub fn icon_rgba(icon: ToolbarIcon, px: u32, ink: [u8; 4]) -> (Vec<u8>, u32, u32) {
    if px == 0 {
        return (Vec::new(), 0, 0);
    }
    let side = px as f32;
    let shapes: Vec<Shape> = shapes_for(icon)
        .into_iter()
        .map(|shape| shape.with_min_stroke(MIN_STROKE_PX / side))
        .collect();
    let mut pixels = vec![0u8; (px * px * 4) as usize];
    for y in 0..px {
        for x in 0..px {
            let p = [(x as f32 + 0.5) / side, (y as f32 + 0.5) / side];
            let mut distance = f32::MAX;
            for shape in &shapes {
                distance = distance.min(shape.distance(p));
            }
            // Distances are in unit space; one pixel is `1.0 / side` of it.
            let coverage = (0.5 - distance * side).clamp(0.0, 1.0);
            if coverage <= 0.0 {
                continue;
            }
            let alpha = (f32::from(ink[3]) * coverage).round().clamp(0.0, 255.0) as u8;
            let offset = ((y * px + x) * 4) as usize;
            pixels[offset] = ink[0];
            pixels[offset + 1] = ink[1];
            pixels[offset + 2] = ink[2];
            pixels[offset + 3] = alpha;
        }
    }
    (pixels, px, px)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(count: usize) -> Vec<ToolbarButton> {
        (0..count)
            .map(|_| ToolbarButton::icon(ToolbarIcon::Line))
            .collect()
    }

    const SCREEN: Rect = [0.0, 0.0, 1920.0, 1080.0];

    #[test]
    fn a_full_size_strip_is_as_wide_as_its_parts() {
        let buttons = row(4);
        let (w, h) = track_extent(&buttons, BUTTON_SIZE);
        assert!((w - (4.0 * BUTTON_SIZE + 3.0 * BUTTON_GAP + 2.0 * PAD_X)).abs() < 1e-3);
        assert!((h - (BUTTON_SIZE + 2.0 * PAD_Y)).abs() < 1e-3);
    }

    #[test]
    fn a_label_cell_is_wider_than_an_icon_cell() {
        let mut buttons = row(2);
        buttons.push(ToolbarButton::label("1920x1080"));
        let icon = button_rect(
            place(
                [100.0, 100.0, 400.0, 300.0],
                SCREEN,
                track_extent(&buttons, BUTTON_SIZE),
            ),
            &buttons,
            BUTTON_SIZE,
            0,
        )
        .unwrap();
        let label = button_rect(
            place(
                [100.0, 100.0, 400.0, 300.0],
                SCREEN,
                track_extent(&buttons, BUTTON_SIZE),
            ),
            &buttons,
            BUTTON_SIZE,
            2,
        )
        .unwrap();
        assert!(label[2] > icon[2]);
        assert!((label[2] - icon[2] * LABEL_UNITS).abs() < 1e-3);
    }

    #[test]
    fn buttons_never_leave_the_track() {
        let buttons = row(20);
        let size = fit_button_size(&buttons, SCREEN[2] - 2.0 * SCREEN_MARGIN);
        let bar = place(
            [200.0, 200.0, 800.0, 400.0],
            SCREEN,
            track_extent(&buttons, size),
        );
        for index in 0..buttons.len() {
            let [x, y, w, h] = button_rect(bar, &buttons, size, index).expect("button in range");
            assert!(
                x >= bar[0] - 1e-3,
                "button {index} starts left of the track"
            );
            assert!(
                x + w <= bar[0] + bar[2] + 1e-3,
                "button {index} runs past the track"
            );
            assert!(y >= bar[1] - 1e-3 && y + h <= bar[1] + bar[3] + 1e-3);
        }
        assert_eq!(button_rect(bar, &buttons, size, buttons.len()), None);
    }

    #[test]
    fn slots_tile_the_track_without_seams() {
        let buttons = {
            let mut b = row(3);
            b.insert(1, ToolbarButton::label("640x480"));
            b
        };
        let bar = place(
            [0.0, 0.0, 600.0, 400.0],
            SCREEN,
            track_extent(&buttons, BUTTON_SIZE),
        );
        let mut previous_right = bar[0];
        for index in 0..buttons.len() {
            let [x, _, w, _] = slot_rect(bar, &buttons, BUTTON_SIZE, index).expect("slot in range");
            assert!(
                (x - previous_right).abs() < 1e-3,
                "seam before slot {index}: {x} vs {previous_right}"
            );
            previous_right = x + w;
        }
        assert!((previous_right - (bar[0] + bar[2])).abs() < 1e-3);
    }

    #[test]
    fn a_painted_button_sits_inside_the_slot_that_hit_tests_it() {
        let buttons = row(6);
        let bar = place(
            [0.0, 0.0, 600.0, 400.0],
            SCREEN,
            track_extent(&buttons, BUTTON_SIZE),
        );
        for index in 0..buttons.len() {
            let [sx, sy, sw, sh] = slot_rect(bar, &buttons, BUTTON_SIZE, index).unwrap();
            let [bx, by, bw, bh] = button_rect(bar, &buttons, BUTTON_SIZE, index).unwrap();
            assert!(
                bx >= sx - 1e-3 && bx + bw <= sx + sw + 1e-3,
                "button {index}"
            );
            assert!(
                by >= sy - 1e-3 && by + bh <= sy + sh + 1e-3,
                "button {index}"
            );
            assert_eq!(
                button_at(bar, &buttons, BUTTON_SIZE, bx + bw * 0.5, by + bh * 0.5),
                Some(index)
            );
        }
    }

    #[test]
    fn clicks_outside_the_track_and_on_disabled_cells_hit_nothing() {
        let mut buttons = row(3);
        buttons[1].enabled = false;
        let bar = place(
            [0.0, 0.0, 600.0, 400.0],
            SCREEN,
            track_extent(&buttons, BUTTON_SIZE),
        );
        let [x, y, w, h] = bar;
        assert_eq!(
            button_at(bar, &buttons, BUTTON_SIZE, x - 2.0, y + 2.0),
            None
        );
        assert_eq!(
            button_at(bar, &buttons, BUTTON_SIZE, x + w + 2.0, y + 2.0),
            None
        );
        assert_eq!(
            button_at(bar, &buttons, BUTTON_SIZE, x + 2.0, y - 2.0),
            None
        );
        assert_eq!(
            button_at(bar, &buttons, BUTTON_SIZE, x + 2.0, y + h + 2.0),
            None
        );

        let disabled = button_rect(bar, &buttons, BUTTON_SIZE, 1).unwrap();
        assert_eq!(
            button_at(
                bar,
                &buttons,
                BUTTON_SIZE,
                disabled[0] + disabled[2] * 0.5,
                disabled[1] + disabled[3] * 0.5
            ),
            None,
            "a disabled cell must let the click through"
        );
        // …but it is still part of the strip, so a press there is the
        // toolbar's and must not start drawing on the canvas.
        assert!(hits_toolbar(
            bar,
            disabled[0] + disabled[2] * 0.5,
            disabled[1] + disabled[3] * 0.5
        ));
    }

    #[test]
    fn the_strip_prefers_the_space_under_the_selection() {
        let buttons = row(5);
        let extent = track_extent(&buttons, BUTTON_SIZE);
        let selection = [500.0, 300.0, 400.0, 200.0];
        let bar = place(selection, SCREEN, extent);
        assert!(
            bar[1] >= selection[1] + selection[3],
            "strip must sit below"
        );
        // Centred on the selection.
        assert!(
            ((bar[0] + bar[2] * 0.5) - (selection[0] + selection[2] * 0.5)).abs() <= 1.0,
            "strip must be centred on the selection"
        );
    }

    #[test]
    fn a_selection_against_the_bottom_pushes_the_strip_above_it() {
        let buttons = row(5);
        let extent = track_extent(&buttons, BUTTON_SIZE);
        let selection = [500.0, 900.0, 400.0, 175.0];
        let bar = place(selection, SCREEN, extent);
        assert!(
            bar[1] + bar[3] <= selection[1],
            "strip must sit above the selection, got y={} h={}",
            bar[1],
            bar[3]
        );
    }

    #[test]
    fn a_fullscreen_selection_keeps_the_strip_on_screen() {
        let buttons = row(20);
        let size = fit_button_size(&buttons, SCREEN[2] - 2.0 * SCREEN_MARGIN);
        let extent = track_extent(&buttons, size);
        let bar = place([0.0, 0.0, 1920.0, 1080.0], SCREEN, extent);
        assert!(bar[0] >= SCREEN_MARGIN - 1e-3);
        assert!(bar[1] >= SCREEN_MARGIN - 1e-3);
        assert!(bar[0] + bar[2] <= SCREEN[2] - SCREEN_MARGIN + 1e-3);
        assert!(bar[1] + bar[3] <= SCREEN[3] - SCREEN_MARGIN + 1e-3);
    }

    #[test]
    fn a_selection_at_the_left_edge_does_not_push_the_strip_off_screen() {
        let buttons = row(20);
        let size = fit_button_size(&buttons, SCREEN[2] - 2.0 * SCREEN_MARGIN);
        let extent = track_extent(&buttons, size);
        let bar = place([0.0, 100.0, 40.0, 40.0], SCREEN, extent);
        assert!(bar[0] >= SCREEN_MARGIN - 1e-3);
        assert!(bar[0] + bar[2] <= SCREEN[2] - SCREEN_MARGIN + 1e-3);
    }

    #[test]
    fn buttons_shrink_only_as_far_as_they_stay_legible() {
        let buttons = row(20);
        assert_eq!(fit_button_size(&buttons, 10_000.0), BUTTON_SIZE);
        let narrow = fit_button_size(&buttons, 300.0);
        assert_eq!(narrow, MIN_BUTTON_SIZE);
        let middling = fit_button_size(&buttons, 600.0);
        assert!(middling > MIN_BUTTON_SIZE && middling < BUTTON_SIZE);
        assert!(track_extent(&buttons, middling).0 <= 600.0 + 1e-3);
    }

    #[test]
    fn degenerate_geometry_yields_no_rectangles_instead_of_inverted_ones() {
        let buttons = row(3);
        assert_eq!(
            button_rect([0.0, 0.0, 0.0, 0.0], &buttons, BUTTON_SIZE, 0),
            None
        );
        assert_eq!(
            button_rect([f32::NAN, 0.0, 100.0, 40.0], &buttons, BUTTON_SIZE, 0),
            None
        );
        assert_eq!(slot_rect([0.0, 0.0, 100.0, 40.0], &buttons, 0.0, 0), None);
        assert_eq!(
            button_at([0.0, 0.0, 100.0, 40.0], &[], BUTTON_SIZE, 5.0, 5.0),
            None
        );
        assert_eq!(fit_button_size(&buttons, f32::NAN), BUTTON_SIZE);
        assert_eq!(fit_button_size(&[], 500.0), BUTTON_SIZE);
    }

    #[test]
    fn every_icon_rasterises_to_something_visible_and_correctly_tinted() {
        const ICONS: [ToolbarIcon; 19] = [
            ToolbarIcon::Pencil,
            ToolbarIcon::Line,
            ToolbarIcon::Arrow,
            ToolbarIcon::RectOutline,
            ToolbarIcon::RectFilled,
            ToolbarIcon::Ellipse,
            ToolbarIcon::Marker,
            ToolbarIcon::Text,
            ToolbarIcon::Counter,
            ToolbarIcon::Pixelate,
            ToolbarIcon::Invert,
            ToolbarIcon::Thinner,
            ToolbarIcon::Thicker,
            ToolbarIcon::Color,
            ToolbarIcon::Undo,
            ToolbarIcon::Redo,
            ToolbarIcon::Copy,
            ToolbarIcon::Save,
            ToolbarIcon::Close,
        ];
        for icon in ICONS {
            for px in [MIN_BUTTON_SIZE as u32, 16, 24, 64] {
                let (pixels, w, h) = icon_rgba(icon, px, [200, 40, 90, 255]);
                assert_eq!((w, h), (px, px), "{icon:?} at {px}px");
                assert_eq!(pixels.len(), (px * px * 4) as usize);
                let opaque = pixels.chunks_exact(4).filter(|p| p[3] > 200).count();
                assert!(opaque > 0, "{icon:?} at {px}px rasterised to nothing");
                // Ink is never written into a fully transparent pixel's color,
                // and every covered pixel carries exactly the requested ink.
                for p in pixels.chunks_exact(4).filter(|p| p[3] > 0) {
                    assert_eq!([p[0], p[1], p[2]], [200, 40, 90], "{icon:?} tint");
                }
            }
        }
    }

    #[test]
    fn an_icon_stays_inside_its_square() {
        // A glyph that bled to the edge would touch the button's rim.
        for icon in [
            ToolbarIcon::Ellipse,
            ToolbarIcon::Close,
            ToolbarIcon::Save,
            ToolbarIcon::Counter,
        ] {
            let px = 64;
            let (pixels, ..) = icon_rgba(icon, px, [255, 255, 255, 255]);
            for i in 0..px {
                for (x, y) in [(0, i), (px - 1, i), (i, 0), (i, px - 1)] {
                    let alpha = pixels[((y * px + x) * 4 + 3) as usize];
                    assert_eq!(alpha, 0, "{icon:?} touches its border at {x},{y}");
                }
            }
        }
    }

    /// Exercise the contact-sheet compositor on every icon and size entirely
    /// in memory, keeping ordinary test runs deterministic and side-effect free.
    #[test]
    fn icon_contact_sheet_composites_every_icon_at_every_size() {
        const ICONS: [ToolbarIcon; 19] = [
            ToolbarIcon::Pencil,
            ToolbarIcon::Line,
            ToolbarIcon::Arrow,
            ToolbarIcon::RectOutline,
            ToolbarIcon::RectFilled,
            ToolbarIcon::Ellipse,
            ToolbarIcon::Marker,
            ToolbarIcon::Text,
            ToolbarIcon::Counter,
            ToolbarIcon::Pixelate,
            ToolbarIcon::Invert,
            ToolbarIcon::Thinner,
            ToolbarIcon::Thicker,
            ToolbarIcon::Color,
            ToolbarIcon::Undo,
            ToolbarIcon::Redo,
            ToolbarIcon::Copy,
            ToolbarIcon::Save,
            ToolbarIcon::Close,
        ];
        let sizes = [16u32, 24, 48];
        let cell = 56u32;
        let width = cell * ICONS.len() as u32;
        let height = cell * sizes.len() as u32;
        let mut sheet = image::RgbaImage::from_pixel(width, height, image::Rgba([32, 30, 40, 255]));
        let mut covered_pixels = 0usize;
        for (row, px) in sizes.iter().enumerate() {
            for (col, icon) in ICONS.iter().enumerate() {
                let (pixels, w, h) = icon_rgba(*icon, *px, [235, 235, 245, 255]);
                let ox = col as u32 * cell + (cell - w) / 2;
                let oy = row as u32 * cell + (cell - h) / 2;
                let mut cell_covered_pixels = 0usize;
                for y in 0..h {
                    for x in 0..w {
                        let o = ((y * w + x) * 4) as usize;
                        let a = f32::from(pixels[o + 3]) / 255.0;
                        if a > 0.0 {
                            cell_covered_pixels += 1;
                        }
                        let dst = sheet.get_pixel_mut(ox + x, oy + y);
                        for c in 0..3 {
                            dst[c] = (f32::from(dst[c]) * (1.0 - a) + f32::from(pixels[o + c]) * a)
                                as u8;
                        }
                    }
                }
                assert!(
                    cell_covered_pixels > 0,
                    "{icon:?} at {px}px contributed no contact-sheet pixels"
                );
                assert!(
                    (0..h).any(|y| (0..w).any(|x| {
                        let pixel = sheet.get_pixel(ox + x, oy + y);
                        [pixel[0], pixel[1], pixel[2]] != [32, 30, 40]
                    })),
                    "{icon:?} at {px}px did not alter its contact-sheet cell"
                );
                covered_pixels += cell_covered_pixels;
            }
        }
        assert_eq!(sheet.dimensions(), (width, height));
        assert!(covered_pixels > ICONS.len() * sizes.len());
    }

    #[test]
    fn a_zero_size_icon_is_empty_rather_than_a_panic() {
        let (pixels, w, h) = icon_rgba(ToolbarIcon::Line, 0, [255, 255, 255, 255]);
        assert!(pixels.is_empty());
        assert_eq!((w, h), (0, 0));
    }

    #[test]
    fn icon_and_label_metrics_survive_nonsense_sizes() {
        assert_eq!(icon_extent(f32::NAN), 0);
        assert!(icon_extent(BUTTON_SIZE) > 0);
        assert!(label_font_size(f32::NAN) > 0.0);
        assert!(label_font_size(MIN_BUTTON_SIZE) >= 7.0);
        assert_eq!(pill_radius(30.0), 15.0);
        assert_eq!(pill_radius(f32::NAN), 0.0);
    }
}
