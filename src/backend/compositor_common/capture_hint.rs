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
            format!("Screenshot · {target} · click window · drag region · Esc")
        }
    } else if armed {
        format!("Recording · {target} · Enter to start · drag handles · Esc")
    } else if let Some(title) = probe.as_deref() {
        format!("Recording · {title} · click to pick · Enter · Esc")
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
        let shot = capture_hint_label(true, "window", false, None);
        assert!(shot.contains("Screenshot"));
        assert!(shot.contains("window"));
        assert!(shot.contains("Esc"));

        let rec = capture_hint_label(false, "region", false, None);
        assert!(rec.contains("Recording"));
        assert!(rec.contains("Enter"));

        let armed = capture_hint_label(false, "window", true, None);
        assert!(armed.contains("Enter to start"));

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
        let layout = capture_hint_layout(200.0, 100.0, 80.0, 12.0);
        // chip_w = 80 + 2*HINT_PAD_X = 112 → centered at (200-112)/2 = 44
        assert!((layout.chip[0] - 44.0).abs() < f32::EPSILON);
        assert!((layout.chip[1] - (100.0 - HINT_MARGIN - 28.0)).abs() < f32::EPSILON);
        assert_eq!(layout.chip[2], 80.0 + 2.0 * HINT_PAD_X);
        assert_eq!(layout.chip[3], 12.0 + 2.0 * HINT_PAD_Y);
    }
}
