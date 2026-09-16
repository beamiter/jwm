//! On-screen hint chip while interactive screenshot / recording selection is
//! armed. Mirrors the REC chip's flat pill language so capture guidance reads
//! as chrome, not a toast or timed OSD.

/// Gap between the chip and the bottom edge.
pub(crate) const HINT_MARGIN: f32 = 20.0;
/// Horizontal padding inside the pill.
pub(crate) const HINT_PAD_X: f32 = 16.0;
/// Vertical padding inside the pill.
pub(crate) const HINT_PAD_Y: f32 = 8.0;

/// Build the selection-phase label. `target` is [`CaptureTarget::label`].
/// `armed` is true once recording has a committed region (Enter will start).
#[must_use]
pub(crate) fn capture_hint_label(screenshot: bool, target: &str, armed: bool) -> String {
    if screenshot {
        format!("Screenshot · {target} · click window · drag region · Esc")
    } else if armed {
        format!("Recording · {target} · Enter to start · drag handles · Esc")
    } else {
        format!("Recording · {target} · click window · drag region · Enter · Esc")
    }
}

/// Bottom-center pill layout for a rasterized label of `text_w` × `text_h`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct CaptureHintLayout {
    pub(crate) chip: [f32; 4],
    pub(crate) text: [f32; 4],
}

#[must_use]
pub(crate) fn capture_hint_layout(
    screen_w: f32,
    screen_h: f32,
    text_w: f32,
    text_h: f32,
) -> CaptureHintLayout {
    let chip_w = text_w + 2.0 * HINT_PAD_X;
    let chip_h = text_h + 2.0 * HINT_PAD_Y;
    let x = ((screen_w - chip_w) * 0.5).max(0.0);
    let y = (screen_h - HINT_MARGIN - chip_h).max(0.0);
    CaptureHintLayout {
        chip: [x, y, chip_w, chip_h],
        text: [x + HINT_PAD_X, y + HINT_PAD_Y, text_w, text_h],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_name_the_mode_and_primary_actions() {
        let shot = capture_hint_label(true, "window", false);
        assert!(shot.contains("Screenshot"));
        assert!(shot.contains("window"));
        assert!(shot.contains("Esc"));

        let rec = capture_hint_label(false, "region", false);
        assert!(rec.contains("Recording"));
        assert!(rec.contains("Enter"));

        let armed = capture_hint_label(false, "window", true);
        assert!(armed.contains("Enter to start"));
    }

    #[test]
    fn layout_centers_on_the_bottom_edge() {
        let layout = capture_hint_layout(200.0, 100.0, 80.0, 12.0);
        // chip_w = 80 + 2*HINT_PAD_X = 112 → centered at (200-112)/2 = 44
        assert!((layout.chip[0] - 44.0).abs() < f32::EPSILON);
        assert!((layout.chip[1] - (100.0 - HINT_MARGIN - 28.0)).abs() < f32::EPSILON);
        assert_eq!(layout.chip[2], 80.0 + 2.0 * HINT_PAD_X);
        assert_eq!(layout.chip[3], 12.0 + 2.0 * HINT_PAD_Y);
    }
}
