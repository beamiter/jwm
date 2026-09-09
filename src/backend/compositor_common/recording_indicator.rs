//! Backend-neutral "recording in progress" chips.
//!
//! A screen recording used to be discoverable only over IPC: nothing on
//! screen answered "did it actually start?" or warned that every pixel is
//! still being encoded. The REC chip is the persistent cue — a red dot and a
//! running clock parked in the bottom-right corner for as long as the capture
//! pipeline reports itself active. The MIC chip is the same cue for
//! standalone audio recording: the recorder lives WM-side, so the compositor
//! cannot derive its state the way it derives the REC chip's — the WM pushes
//! it, and the chip carries a static `MIC` label (no clock, one raster per
//! state change, no frame pump). When both recordings run together the MIC
//! chip parks directly above the REC chip so the two never overlap.
//!
//! Both compositors draw the chips *after* the frame's screenshot and
//! recording readbacks (the same slot the interactive crop outline uses), so
//! they are visible on the local output but can never leak into a PNG or the
//! encoded video. Everything that is not GL — the label text and the chip
//! geometry — lives here so the two compositors cannot drift.

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
/// Vertical gap between the two recording cues when screen and standalone
/// audio recording share the bottom-right corner.
pub(crate) const CHIP_STACK_GAP: f32 = 10.0;

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

/// The standalone-audio-recording chip's label: `MIC` while the WM-side
/// recorder runs, `None` otherwise — the renderer draws nothing and frees
/// the label texture.
///
/// The label is deliberately static. The REC chip re-rasterizes because its
/// `m:ss` clock flips the shown second; a fixed label rasterizes once per
/// state change and needs no frame pump, so this function takes no elapsed
/// time by construction.
pub(crate) fn mic_indicator_label(active: bool) -> Option<&'static str> {
    active.then_some("MIC")
}

/// Lay the MIC chip out for a rasterized label of `text_w` × `text_h`. The
/// chip takes the REC slot — bottom-right, `CHIP_MARGIN` from the corner —
/// when the corner is free, and parks directly above the REC chip (same
/// right margin, `CHIP_STACK_GAP` between the pills) when both recordings
/// run together: `rec_chip_h` is the height the REC chip drew this frame,
/// `None` when it is not up. The REC chip's own slot never moves.
pub(crate) fn mic_indicator_layout(
    screen_w: f32,
    screen_h: f32,
    text_w: f32,
    text_h: f32,
    rec_chip_h: Option<f32>,
) -> RecordingIndicatorLayout {
    let mut layout = recording_indicator_layout(screen_w, screen_h, text_w, text_h);
    if let Some(rec_chip_h) = rec_chip_h {
        // A screen too short to hold both chips pins the MIC chip to the top
        // edge rather than pushing it off-screen: the cue staying visible
        // matters more than the degenerate-case overlap, the same honesty
        // as the REC chip's origin clamp.
        let lift = (rec_chip_h + CHIP_STACK_GAP).min(layout.chip[1]);
        layout.chip[1] -= lift;
        layout.dot[1] -= lift;
        layout.text[1] -= lift;
    }
    layout
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

    #[test]
    fn the_mic_label_is_static() {
        // No recording, no chip — the same transition rule as the REC chip.
        assert_eq!(mic_indicator_label(false), None);
        // The label never carries a clock: one raster per state change, and
        // nothing ever repumps frames for the digits.
        assert_eq!(mic_indicator_label(true), Some("MIC"));
    }

    #[test]
    fn the_mic_chip_takes_the_rec_slot_when_the_corner_is_free() {
        // Standalone audio recording alone: the chip sits exactly where the
        // REC chip would, byte for byte.
        let mic = mic_indicator_layout(1920.0, 1080.0, 48.0, 19.0, None);
        assert_eq!(mic, recording_indicator_layout(1920.0, 1080.0, 48.0, 19.0));
    }

    #[test]
    fn the_mic_chip_stacks_above_the_rec_chip_without_overlapping() {
        let rec = recording_indicator_layout(1920.0, 1080.0, 60.0, 19.0);
        let mic = mic_indicator_layout(1920.0, 1080.0, 48.0, 19.0, Some(rec.chip[3]));

        // Same right margin, `CHIP_STACK_GAP` between the pills, and the REC
        // chip's slot is whatever it would have been on its own.
        assert_eq!(mic.chip[0] + mic.chip[2] + CHIP_MARGIN, 1920.0);
        assert_eq!(mic.chip[1] + mic.chip[3] + CHIP_STACK_GAP, rec.chip[1]);
        // Dot and label ride the same lift, so their centers stay inside.
        let chip_mid = mic.chip[1] + mic.chip[3] / 2.0;
        assert!((mic.dot[1] + mic.dot[3] / 2.0 - chip_mid).abs() < 1e-4);
        assert!((mic.text[1] + mic.text[3] / 2.0 - chip_mid).abs() < 1e-4);
        // No overlap on any screen tall enough to hold both chips.
        assert!(mic.chip[1] + mic.chip[3] <= rec.chip[1]);
    }

    #[test]
    fn the_mic_chip_never_leaves_the_screen_on_degenerate_displays() {
        let rec = recording_indicator_layout(200.0, 70.0, 60.0, 19.0);
        let mic = mic_indicator_layout(200.0, 70.0, 48.0, 19.0, Some(rec.chip[3]));
        // Too short for both pills plus the gap: the MIC chip pins to the
        // top edge (fully visible) instead of sliding off-screen.
        assert_eq!(mic.chip[1], 0.0);
        assert_eq!(mic.dot[1], (mic.chip[3] - CHIP_DOT) / 2.0);
    }
}
