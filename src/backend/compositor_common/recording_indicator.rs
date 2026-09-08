//! Backend-neutral "recording in progress" chip.
//!
//! A screen recording used to be discoverable only over IPC: nothing on
//! screen answered "did it actually start?" or warned that every pixel is
//! still being encoded. This chip is the persistent cue — a red dot and a
//! running clock parked in the bottom-right corner for as long as the capture
//! pipeline reports itself active.
//!
//! Both compositors draw it *after* the frame's screenshot and recording
//! readbacks (the same slot the interactive crop outline uses), so it is
//! visible on the local output but can never leak into a PNG or the encoded
//! video. Everything that is not GL — the label text and the chip geometry —
//! lives here so the two compositors cannot drift.

use std::time::Duration;

/// Gap between the chip and the screen corner.
pub(crate) const CHIP_MARGIN: f32 = 16.0;
/// Horizontal padding on each side of the chip's contents.
pub(crate) const CHIP_PAD_X: f32 = 14.0;
/// Vertical padding above and below the label.
pub(crate) const CHIP_PAD_Y: f32 = 7.0;
/// Recording dot diameter.
pub(crate) const CHIP_DOT: f32 = 9.0;
/// Space between the dot and the label.
pub(crate) const CHIP_DOT_GAP: f32 = 8.0;

/// The recording red the interactive crop outline already draws with
/// (`render_recording_region_overlay`), so both recording cues read as one
/// feature.
pub(crate) const DOT_COLOR: [f32; 4] = [1.0, 0.2, 0.12, 0.95];

/// Rects the renderer draws, in screen coordinates (top-left origin).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct RecordingIndicatorLayout {
    /// The pill background.
    pub(crate) chip: [f32; 4],
    /// The red dot, vertically centered on the chip's left.
    pub(crate) dot: [f32; 4],
    /// The rasterized label quad, vertically centered after the dot.
    pub(crate) text: [f32; 4],
}

/// The chip's label while recording: `REC` plus the running clock when the
/// capture pipeline knows its start time. `None` while no recording is
/// active — the renderer draws nothing and frees the label texture.
///
/// The clock ticks at the recording's own frame pacing, which is what keeps
/// both compositors repainting while a recording runs; no separate timer is
/// needed for the digits to advance.
pub(crate) fn recording_indicator_label(active: bool, elapsed: Option<Duration>) -> Option<String> {
    if !active {
        return None;
    }
    Some(match elapsed {
        Some(elapsed) => format!("REC {}", format_elapsed(elapsed)),
        None => "REC".to_string(),
    })
}

/// `m:ss` under an hour, `h:mm:ss` past it — the running clock never widens
/// mid-recording once hours appear.
fn format_elapsed(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    let (hours, minutes, seconds) = (secs / 3600, (secs / 60) % 60, secs % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// Lay the chip out in the bottom-right corner for a rasterized label of
/// `text_w` × `text_h`. The chip grows with the label (a hours-long recording
/// widens it) and clamps into the screen when the corner is too small to
/// hold it.
pub(crate) fn recording_indicator_layout(
    screen_w: f32,
    screen_h: f32,
    text_w: f32,
    text_h: f32,
) -> RecordingIndicatorLayout {
    let chip_w = CHIP_PAD_X + CHIP_DOT + CHIP_DOT_GAP + text_w + CHIP_PAD_X;
    let chip_h = (text_h + 2.0 * CHIP_PAD_Y).max(CHIP_DOT + 2.0 * CHIP_PAD_Y);
    let x = (screen_w - CHIP_MARGIN - chip_w).max(0.0);
    let y = (screen_h - CHIP_MARGIN - chip_h).max(0.0);
    RecordingIndicatorLayout {
        chip: [x, y, chip_w, chip_h],
        dot: [
            x + CHIP_PAD_X,
            y + (chip_h - CHIP_DOT) / 2.0,
            CHIP_DOT,
            CHIP_DOT,
        ],
        text: [
            x + CHIP_PAD_X + CHIP_DOT + CHIP_DOT_GAP,
            y + (chip_h - text_h) / 2.0,
            text_w,
            text_h,
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_recording_means_no_chip() {
        // The state transition that clears the indicator: whatever the clock
        // last said, an inactive pipeline draws nothing.
        assert_eq!(recording_indicator_label(false, None), None);
        assert_eq!(
            recording_indicator_label(false, Some(Duration::from_secs(5))),
            None
        );
    }

    #[test]
    fn the_label_carries_a_running_clock() {
        assert_eq!(
            recording_indicator_label(true, Some(Duration::from_secs(7))).as_deref(),
            Some("REC 0:07")
        );
        assert_eq!(
            recording_indicator_label(true, Some(Duration::from_secs(83))).as_deref(),
            Some("REC 1:23")
        );
        assert_eq!(
            recording_indicator_label(true, Some(Duration::from_secs(3599))).as_deref(),
            Some("REC 59:59")
        );
        assert_eq!(
            recording_indicator_label(true, Some(Duration::from_secs(3600))).as_deref(),
            Some("REC 1:00:00")
        );
        assert_eq!(
            recording_indicator_label(true, Some(Duration::from_secs(3661))).as_deref(),
            Some("REC 1:01:01")
        );
        // A sub-second recording still reads as a started clock.
        assert_eq!(
            recording_indicator_label(true, Some(Duration::from_millis(400))).as_deref(),
            Some("REC 0:00")
        );
        // The pipeline started but never reported a start time: dot + REC.
        assert_eq!(
            recording_indicator_label(true, None).as_deref(),
            Some("REC")
        );
    }

    #[test]
    fn the_chip_parks_in_the_bottom_right_corner() {
        let layout = recording_indicator_layout(1920.0, 1080.0, 60.0, 19.0);
        let [x, y, w, h] = layout.chip;

        assert_eq!(x + w + CHIP_MARGIN, 1920.0);
        assert_eq!(y + h + CHIP_MARGIN, 1080.0);
        // Contents sit inside the chip: dot on the left, label after it.
        assert!(layout.dot[0] >= x && layout.dot[0] + layout.dot[2] <= layout.text[0]);
        assert!(layout.text[0] + layout.text[2] <= x + w);
        assert!(layout.dot[1] >= y && layout.dot[1] + layout.dot[3] <= y + h);
        assert!(layout.text[1] >= y && layout.text[1] + layout.text[3] <= y + h);
        // Vertically centered contents.
        assert!((layout.dot[1] + layout.dot[3] / 2.0 - (y + h / 2.0)).abs() < 1e-4);
        assert!((layout.text[1] + layout.text[3] / 2.0 - (y + h / 2.0)).abs() < 1e-4);
    }

    #[test]
    fn the_chip_widens_for_long_recordings_and_clamps_to_tiny_screens() {
        let short = recording_indicator_layout(1920.0, 1080.0, 50.0, 19.0);
        let long = recording_indicator_layout(1920.0, 1080.0, 90.0, 19.0);
        assert!(long.chip[2] > short.chip[2]);
        assert_eq!(short.chip[3], long.chip[3]);

        // A screen smaller than the chip still shows it, pinned to the origin.
        let tiny = recording_indicator_layout(10.0, 8.0, 60.0, 19.0);
        assert_eq!(tiny.chip[0], 0.0);
        assert_eq!(tiny.chip[1], 0.0);
    }
}
