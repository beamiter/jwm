//! On-screen hint chip while interactive screenshot / recording selection is
//! armed. Mirrors the REC chip's flat pill language so capture guidance reads
//! as chrome, not a toast or timed OSD.

/// Gap between the chip and the bottom edge.
pub(crate) const HINT_MARGIN: f32 = 20.0;
/// Horizontal padding inside the pill.
pub(crate) const HINT_PAD_X: f32 = 16.0;
/// Vertical padding inside the pill.
pub(crate) const HINT_PAD_Y: f32 = 8.0;
/// Soft-probe titles longer than this are ellipsized in the chip.
pub(crate) const HINT_TITLE_MAX_CHARS: usize = 28;

/// Ellipsize a window title for the selection hint chip.
#[must_use]
pub(crate) fn truncate_hint_title(title: &str) -> String {
    let trimmed = title.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let count = trimmed.chars().count();
    if count <= HINT_TITLE_MAX_CHARS {
        return trimmed.to_string();
    }
    let keep = HINT_TITLE_MAX_CHARS.saturating_sub(1);
    let mut out: String = trimmed.chars().take(keep).collect();
    out.push('…');
    out
}

/// Build the selection-phase label. `target` is [`CaptureTarget::label`].
/// `armed` is true once recording has a committed region (Enter will start).
/// `probe` is an optional soft-probed window title under the pointer.
#[must_use]
pub(crate) fn capture_hint_label(
    screenshot: bool,
    target: &str,
    armed: bool,
    probe: Option<&str>,
) -> String {
    let probe = probe
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(truncate_hint_title);
    if screenshot {
        if let Some(title) = probe.as_deref() {
            format!("Screenshot · {title} · click to pick · Esc")
        } else {
            format!("Screenshot · {target} · click window · drag region · Tab/middle cycle · Esc")
        }
    } else if armed {
        format!("Recording · {target} · Enter / Space / double-click · Esc")
    } else if let Some(title) = probe.as_deref() {
        format!("Recording · {title} · click to pick · Enter / Space · Esc")
    } else {
        format!(
            "Recording · {target} · click window · drag · Tab/middle cycle · Enter / Space · Esc"
        )
    }
}

fn hint_pad_x(chip_w: f32) -> f32 {
    if !(chip_w.is_finite() && chip_w > 0.0) {
        return 0.0;
    }
    if chip_w < 2.0 * HINT_PAD_X {
        HINT_PAD_X.min(chip_w * 0.12).max(2.0)
    } else {
        HINT_PAD_X
    }
}

fn hint_pad_y(chip_h: f32) -> f32 {
    if !(chip_h.is_finite() && chip_h > 0.0) {
        return 0.0;
    }
    if chip_h < 2.0 * HINT_PAD_Y {
        HINT_PAD_Y.min(chip_h * 0.2).max(1.0)
    } else {
        HINT_PAD_Y
    }
}

fn hint_margin(screen_h: f32) -> f32 {
    if !(screen_h.is_finite() && screen_h > 0.0) {
        return 0.0;
    }
    if screen_h < 4.0 * HINT_MARGIN {
        HINT_MARGIN.min(screen_h * 0.08).max(0.0)
    } else {
        HINT_MARGIN
    }
}

/// Extra lift so the center hint clears the bottom-right REC / MIC stack.
/// `rec_h` / `mic_h` are the chip heights drawn this frame (`None` when absent).
#[must_use]
pub(crate) fn capture_hint_bottom_lift(rec_h: Option<f32>, mic_h: Option<f32>) -> f32 {
    use super::recording_indicator::{CHIP_MARGIN, CHIP_STACK_GAP};

    let stack = match (rec_h, mic_h) {
        (Some(r), Some(m)) => CHIP_MARGIN + r + CHIP_STACK_GAP + m,
        (Some(r), None) => CHIP_MARGIN + r,
        (None, Some(m)) => CHIP_MARGIN + m,
        (None, None) => return 0.0,
    };
    (stack + CHIP_STACK_GAP - HINT_MARGIN).max(0.0)
}

/// Bottom-center pill layout for a rasterized label of `text_w` × `text_h`.
/// `bottom_lift` raises the chip above bottom-right chrome (REC / MIC).
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
    bottom_lift: f32,
) -> CaptureHintLayout {
    let chip_w = (text_w + 2.0 * HINT_PAD_X).min(screen_w.max(0.0));
    let chip_h = (text_h + 2.0 * HINT_PAD_Y).min(screen_h.max(0.0));
    let pad_x = hint_pad_x(chip_w);
    let pad_y = hint_pad_y(chip_h);
    let margin = hint_margin(screen_h);
    let x = ((screen_w - chip_w) * 0.5).clamp(0.0, (screen_w - chip_w).max(0.0));
    let y = (screen_h - margin - bottom_lift.max(0.0) - chip_h)
        .clamp(0.0, (screen_h - chip_h).max(0.0));
    let text_w = text_w.min((chip_w - 2.0 * pad_x).max(0.0));
    let text_h = text_h.min((chip_h - 2.0 * pad_y).max(0.0));
    let text_x = x + pad_x.min(chip_w);
    let text_y = y + pad_y.min(chip_h);
    CaptureHintLayout {
        chip: [x, y, chip_w, chip_h],
        text: [text_x, text_y, text_w, text_h],
    }
}

#[cfg(test)]
mod tests {
    use super::super::recording_indicator::{CHIP_MARGIN, CHIP_STACK_GAP};
    use super::*;

    #[test]
    fn labels_name_the_mode_and_primary_actions() {
        let shot = capture_hint_label(true, "window", false, None);
        assert!(shot.contains("Screenshot"));
        assert!(shot.contains("window"));
        assert!(shot.contains("Esc"));
        assert!(shot.contains("Tab/middle cycle"));

        let rec = capture_hint_label(false, "region", false, None);
        assert!(rec.contains("Recording"));
        assert!(rec.contains("Enter"));
        assert!(rec.contains("Tab/middle cycle"));

        let armed = capture_hint_label(false, "window", true, None);
        assert!(armed.contains("Enter"));
        assert!(armed.contains("double-click"));

        let probed = capture_hint_label(true, "window", false, Some("Firefox"));
        assert!(probed.contains("Firefox"));
        assert!(probed.contains("click to pick"));
    }

    #[test]
    fn long_probe_titles_are_ellipsized() {
        let long = "a".repeat(HINT_TITLE_MAX_CHARS + 8);
        let truncated = truncate_hint_title(&long);
        assert_eq!(truncated.chars().count(), HINT_TITLE_MAX_CHARS);
        assert!(truncated.ends_with('…'));
    }

    #[test]
    fn layout_centers_on_the_bottom_edge() {
        let layout = capture_hint_layout(200.0, 100.0, 80.0, 12.0, 0.0);
        // chip_w = 80 + 2*HINT_PAD_X = 112 → centered at (200-112)/2 = 44
        assert!((layout.chip[0] - 44.0).abs() < f32::EPSILON);
        assert!((layout.chip[1] - (100.0 - HINT_MARGIN - 28.0)).abs() < f32::EPSILON);
        assert_eq!(layout.chip[2], 80.0 + 2.0 * HINT_PAD_X);
        assert_eq!(layout.chip[3], 12.0 + 2.0 * HINT_PAD_Y);
    }

    #[test]
    fn layout_lifts_above_corner_recording_chrome() {
        let lift = capture_hint_bottom_lift(Some(30.0), None);
        assert!((lift - (CHIP_MARGIN + 30.0 + CHIP_STACK_GAP - HINT_MARGIN)).abs() < f32::EPSILON);
        let layout = capture_hint_layout(200.0, 100.0, 80.0, 12.0, lift);
        assert!((layout.chip[1] - (100.0 - HINT_MARGIN - lift - 28.0)).abs() < f32::EPSILON);
        assert_eq!(capture_hint_bottom_lift(None, None), 0.0);
    }

    #[test]
    fn layout_clamps_a_chip_wider_than_the_output() {
        let layout = capture_hint_layout(100.0, 80.0, 200.0, 12.0, 0.0);
        assert!(layout.chip[2] <= 100.0);
        assert!(layout.chip[0] >= 0.0);
        assert!(layout.chip[0] + layout.chip[2] <= 100.0 + f32::EPSILON);
        assert!(layout.text[2] <= layout.chip[2]);
        let tall = capture_hint_layout(80.0, 20.0, 40.0, 40.0, 0.0);
        assert!(tall.chip[3] <= 20.0);
        assert!(tall.chip[1] >= 0.0);
        assert!(tall.chip[1] + tall.chip[3] <= 20.0 + f32::EPSILON);
        assert!(tall.text[3] <= tall.chip[3]);
        let cramped = capture_hint_layout(40.0, 24.0, 80.0, 20.0, 0.0);
        assert!(cramped.text[2] > 0.0, "padding ate the label");
        assert!(cramped.text[0] >= cramped.chip[0]);
        assert!(cramped.text[0] + cramped.text[2] <= cramped.chip[0] + cramped.chip[2] + 1e-3);
    }
}
