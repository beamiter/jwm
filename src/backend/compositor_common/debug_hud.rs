//! Backend-neutral Material styling for the debug HUD.
//!
//! The HUD used to be a black box of green bitmap text glued to the top-left
//! corner. It now reads as the same design as the system-UI card and the
//! toasts: an elevated surface with a drop shadow, a title row with an accent
//! icon and a state chip, a linear progress meter for the frame rate, and a
//! two-tone label/value stat list. Everything that is not GL — tones, metrics,
//! the row model and the layout arithmetic — lives here so the X11 and Wayland
//! compositors cannot drift apart.

/// `(x, y, w, h)` in surface pixels, top-left origin.
pub(crate) type Rect = (f32, f32, f32, f32);

// --- Material tones, shared with the system-UI card ---

/// Card surface. Slightly translucent so the desktop still shows through,
/// which is what keeps a debug overlay from feeling like a modal.
pub(crate) const SURFACE: [f32; 4] = [0.071, 0.082, 0.114, 0.94];
/// Raised fill for the state chip.
pub(crate) const SURFACE_CHIP: [f32; 4] = [0.108, 0.122, 0.165, 1.0];
/// Unfilled part of the frame-rate meter.
pub(crate) const METER_TRACK: [f32; 4] = [1.0, 1.0, 1.0, 0.10];
/// Ambient shadow under the card.
pub(crate) const SHADOW: [f32; 4] = [0.0, 0.0, 0.0, 0.55];

/// Meter fill at or above [`METER_GOOD`] of the refresh target.
pub(crate) const TONE_GOOD: [f32; 4] = [0.31, 0.85, 0.55, 0.95];
/// Meter fill between [`METER_WARN`] and [`METER_GOOD`].
pub(crate) const TONE_WARN: [f32; 4] = [0.98, 0.75, 0.29, 0.95];
/// Meter fill below [`METER_WARN`]: frames are being missed.
pub(crate) const TONE_BAD: [f32; 4] = [0.95, 0.42, 0.45, 0.95];

const METER_GOOD: f32 = 0.90;
const METER_WARN: f32 = 0.60;

// --- Text tones, brightest first ---

pub(crate) const TITLE_INK: [u8; 4] = [214, 228, 255, 255];
pub(crate) const CHIP_INK: [u8; 4] = [150, 162, 186, 255];
pub(crate) const LABEL_INK: [u8; 4] = [136, 148, 172, 255];
pub(crate) const VALUE_INK: [u8; 4] = [232, 238, 250, 255];

// --- Metrics (8dp grid) ---

/// Distance from the screen corner to the card.
pub(crate) const MARGIN: f32 = 24.0;
pub(crate) const CARD_RADIUS: f32 = 16.0;
pub(crate) const CHIP_RADIUS: f32 = 9.0;
pub(crate) const PAD: f32 = 18.0;
pub(crate) const GAP: f32 = 12.0;
/// Space between the label column and the value column.
pub(crate) const GUTTER: f32 = 18.0;
pub(crate) const METER_H: f32 = 4.0;
pub(crate) const SHADOW_SPREAD: f32 = 36.0;
/// Narrowest card, so short stat lists still look like a panel.
const MIN_CONTENT_W: f32 = 260.0;

/// Title glyph (fa-dashboard). Present in every Nerd Font build; the font
/// rasterizer falls back to `?` if the configured face somehow lacks it.
pub(crate) const TITLE_ICON: &str = "\u{f0e4}";

/// One line of the stat list.
pub(crate) enum HudRow {
    /// Dim overline opening a group, preceded by a blank line.
    Section(String),
    /// Label plus its reading.
    Stat(String, String),
}

/// The stat list under the header, built by the backends and rasterized as
/// two columns so labels and values can carry different tones.
#[derive(Default)]
pub(crate) struct HudRows(Vec<HudRow>);

impl HudRows {
    pub(crate) fn section(&mut self, name: &str) {
        self.0.push(HudRow::Section(name.to_ascii_uppercase()));
    }

    pub(crate) fn stat(&mut self, label: &str, value: impl std::fmt::Display) {
        self.0
            .push(HudRow::Stat(label.to_owned(), value.to_string()));
    }

    /// `(labels, values)` as two strings with the same number of lines, so
    /// the two textures share a baseline grid when drawn side by side.
    pub(crate) fn columns(&self) -> (String, String) {
        let mut labels = String::new();
        let mut values = String::new();
        for (index, row) in self.0.iter().enumerate() {
            if index > 0 {
                labels.push('\n');
                values.push('\n');
            }
            match row {
                HudRow::Section(name) => {
                    // A blank line above the overline groups the rows below it.
                    if index > 0 {
                        labels.push('\n');
                        values.push('\n');
                    }
                    labels.push_str(name);
                }
                HudRow::Stat(label, value) => {
                    labels.push_str(label);
                    values.push_str(value);
                }
            }
        }
        (labels, values)
    }
}

/// Meter fraction and tone for `fps` against the display's refresh target.
pub(crate) fn fps_meter(fps: f32, target: f32) -> (f32, [f32; 4]) {
    let target = if target > 1.0 { target } else { 60.0 };
    let ratio = (fps / target).clamp(0.0, 1.0);
    let tone = if ratio >= METER_GOOD {
        TONE_GOOD
    } else if ratio >= METER_WARN {
        TONE_WARN
    } else {
        TONE_BAD
    };
    (ratio, tone)
}

/// Where every piece of the card lands. Sizes come from the rasterized text
/// textures; a `(0.0, 0.0)` size means that section is absent.
pub(crate) struct HudLayout {
    pub(crate) card: Rect,
    pub(crate) title: (f32, f32),
    pub(crate) chip_pill: Rect,
    pub(crate) chip_text: (f32, f32),
    pub(crate) meter_track: Rect,
    pub(crate) meter_fill: Rect,
    pub(crate) labels: (f32, f32),
    pub(crate) values: (f32, f32),
}

impl HudLayout {
    /// Lay the card out at `origin`, sized around the four text textures.
    /// `meter` is the 0..=1 fill fraction from [`fps_meter`].
    pub(crate) fn new(
        origin: (f32, f32),
        title: (f32, f32),
        chip: (f32, f32),
        labels: (f32, f32),
        values: (f32, f32),
        meter: f32,
    ) -> Self {
        let (x, y) = origin;
        let (chip_pill_w, chip_pill_h) = if chip.0 > 0.0 && chip.1 > 0.0 {
            (chip.0 + 18.0, chip.1 + 8.0)
        } else {
            (0.0, 0.0)
        };
        let header_h = title.1.max(chip_pill_h);
        let header_w = title.0
            + if chip_pill_w > 0.0 {
                chip_pill_w + GAP
            } else {
                0.0
            };
        let body_h = labels.1.max(values.1);
        let body_w = labels.0
            + if values.0 > 0.0 {
                GUTTER + values.0
            } else {
                0.0
            };

        let content_w = header_w.max(body_w).max(MIN_CONTENT_W);
        let mut content_h = header_h + GAP + METER_H;
        if body_h > 0.0 {
            content_h += GAP + body_h;
        }

        let card = (x, y, content_w + 2.0 * PAD, content_h + 2.0 * PAD);
        let title_pos = (x + PAD, y + PAD + (header_h - title.1) * 0.5);
        let chip_pill = (
            x + card.2 - PAD - chip_pill_w,
            y + PAD + (header_h - chip_pill_h) * 0.5,
            chip_pill_w,
            chip_pill_h,
        );
        let chip_text = (chip_pill.0 + 9.0, chip_pill.1 + 4.0);
        let meter_y = y + PAD + header_h + GAP;
        let meter_track = (x + PAD, meter_y, content_w, METER_H);
        // A rounded fill narrower than its own diameter renders as a sliver;
        // hold the minimum at one dot so a stalled compositor still shows one.
        let fill_w = (content_w * meter.clamp(0.0, 1.0)).max(METER_H);
        let meter_fill = (x + PAD, meter_y, fill_w, METER_H);
        let body_y = meter_y + METER_H + GAP;

        Self {
            card,
            title: title_pos,
            chip_pill,
            chip_text,
            meter_track,
            meter_fill,
            labels: (x + PAD, body_y),
            values: (x + PAD + labels.0 + GUTTER, body_y),
        }
    }

    /// Shadow rect for the card, offset downward like a Material elevation.
    pub(crate) fn shadow(&self) -> Rect {
        let (x, y, w, h) = self.card;
        (
            x - SHADOW_SPREAD,
            y - SHADOW_SPREAD + 10.0,
            w + 2.0 * SHADOW_SPREAD,
            h + 2.0 * SHADOW_SPREAD,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn columns_keep_matching_line_counts() {
        let mut rows = HudRows::default();
        rows.stat("FPS", "60.0");
        rows.section("Profiler");
        rows.stat("blur", "1.20 / 0.80 / 3.40");
        let (labels, values) = rows.columns();
        assert_eq!(labels.lines().count(), values.lines().count());
        // The section overline occupies the label column alone.
        assert!(labels.contains("PROFILER"));
        assert!(!values.contains("PROFILER"));
    }

    /// Tones are picked, not computed, so identity is what matters.
    fn same_tone(a: [f32; 4], b: [f32; 4]) -> bool {
        a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-6)
    }

    #[test]
    fn meter_tone_tracks_the_refresh_target() {
        let (full, tone) = fps_meter(60.0, 60.0);
        assert!((full - 1.0).abs() < 1e-6);
        assert!(same_tone(tone, TONE_GOOD));
        assert!(same_tone(fps_meter(45.0, 60.0).1, TONE_WARN));
        assert!(same_tone(fps_meter(20.0, 60.0).1, TONE_BAD));
        // A missing or nonsense target falls back to 60 Hz.
        assert!((fps_meter(60.0, 0.0).0 - 1.0).abs() < 1e-6);
    }

    #[test]
    fn layout_grows_with_the_stat_list() {
        let short = HudLayout::new(
            (24.0, 24.0),
            (200.0, 22.0),
            (60.0, 16.0),
            (0.0, 0.0),
            (0.0, 0.0),
            1.0,
        );
        let tall = HudLayout::new(
            (24.0, 24.0),
            (200.0, 22.0),
            (60.0, 16.0),
            (120.0, 180.0),
            (90.0, 180.0),
            1.0,
        );
        assert!(tall.card.3 > short.card.3);
        // The value column sits right of the label column, past the gutter.
        assert!((tall.values.0 - tall.labels.0 - (120.0 + GUTTER)).abs() < 1e-6);
        // The chip is right-aligned inside the card padding.
        let chip_right = tall.chip_pill.0 + tall.chip_pill.2;
        assert!((chip_right - (tall.card.0 + tall.card.2 - PAD)).abs() < 1e-6);
    }
}
