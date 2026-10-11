//! Backend-neutral "recording in progress" chips.
//!
//! A screen recording used to be discoverable only over IPC: nothing on
//! screen answered "did it actually start?" or warned that every pixel is
//! still being encoded. The REC chip is the persistent cue — a red dot and a
//! running clock parked in each visible output’s bottom-right corner while
//! the capture pipeline reports itself active. The MIC chip is the same cue for
//! standalone audio recording: the recorder lives WM-side, so the compositor
//! cannot derive its state the way it derives the REC chip's — the WM pushes
//! it, and the chip carries a static `MIC` label (no clock, one raster per
//! state change, no frame pump). When both recordings run together the MIC
//! chip parks directly above the REC chip so the two never overlap.
//!
//! Both compositors draw the chips *after* the frame's screenshot and
//! recording readbacks (the same slot the interactive crop outline uses), so
//! the ordinary frame readbacks precede these local cues. Transition caches
//! may reuse a previously presented frame, so this ordering alone is not a
//! guarantee that every cached-transition capture excludes the chips. The
//! label text and chip geometry live here so the compositors cannot drift.

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

fn rec_pad_x(chip_w: f32) -> f32 {
    if !(chip_w.is_finite() && chip_w > 0.0) {
        return 0.0;
    }
    if chip_w < 2.0 * CHIP_PAD_X {
        CHIP_PAD_X.min(chip_w * 0.12).max(2.0)
    } else {
        CHIP_PAD_X
    }
}

fn rec_pad_y(chip_h: f32) -> f32 {
    if !(chip_h.is_finite() && chip_h > 0.0) {
        return 0.0;
    }
    if chip_h < 2.0 * CHIP_PAD_Y {
        CHIP_PAD_Y.min(chip_h * 0.2).max(1.0)
    } else {
        CHIP_PAD_Y
    }
}

fn rec_margin(screen: f32) -> f32 {
    if !(screen.is_finite() && screen > 0.0) {
        return 0.0;
    }
    if screen < 4.0 * CHIP_MARGIN {
        CHIP_MARGIN.min(screen * 0.08).max(0.0)
    } else {
        CHIP_MARGIN
    }
}

/// The recording red used by the REC / MIC chips. The interactive crop cue
/// shares the screenshot snap-preview blue instead — these chips stay red so
/// "recording is live" remains distinct from "selecting a source".
#[must_use]
pub(crate) fn dot_color() -> [f32; 4] {
    super::ui_theme::palette().recording_live()
}

/// Slow pulse on the live dot so a static red pill is harder to miss against
/// a matching wallpaper. The trough never goes fully out.
const DOT_PULSE_HZ: f32 = 1.0;
const DOT_ALPHA_FLOOR: f32 = 0.45;

fn rec_dot(chip_h: f32) -> f32 {
    if chip_h.is_finite() && chip_h > 0.0 && chip_h < 4.0 * CHIP_DOT {
        CHIP_DOT.min(chip_h * 0.35).max(3.0).min(chip_h)
    } else {
        CHIP_DOT
    }
}

fn rec_dot_gap(chip_w: f32) -> f32 {
    if chip_w.is_finite() && chip_w > 0.0 && chip_w < 8.0 * CHIP_DOT_GAP {
        CHIP_DOT_GAP.min(chip_w * 0.04).max(2.0)
    } else {
        CHIP_DOT_GAP
    }
}

/// Opacity of the recording / mic dot at `elapsed`. Layout is independent of
/// this — only the fill alpha moves.
#[must_use]
pub(crate) fn dot_alpha(elapsed: Duration) -> f32 {
    let t = elapsed.as_secs_f32();
    let pulse = (t * DOT_PULSE_HZ * std::f32::consts::TAU).sin() * 0.5 + 0.5;
    dot_color()[3] * pulse.max(DOT_ALPHA_FLOOR)
}

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

impl RecordingIndicatorLayout {
    pub(crate) fn translated(mut self, x: f32, y: f32) -> Self {
        for rect in [&mut self.chip, &mut self.dot, &mut self.text] {
            rect[0] += x;
            rect[1] += y;
        }
        self
    }
}

/// Actual visible output rectangles in the existing global framebuffer.
/// A virtual desktop's bottom-right corner can be a hole in an L-shaped
/// layout. Clip each output independently; do not translate negative origins
/// into a new coordinate system or draw mirrored outputs twice.
pub(crate) fn recording_indicator_viewports(
    screen: (u32, u32),
    monitors: impl IntoIterator<Item = (i32, i32, u32, u32)>,
) -> Vec<[f32; 4]> {
    let mut viewports = Vec::new();
    let mut has_monitors = false;
    for (x, y, w, h) in monitors {
        has_monitors = true;
        let left = i64::from(x).max(0);
        let top = i64::from(y).max(0);
        let right = (i64::from(x) + i64::from(w)).min(i64::from(screen.0));
        let bottom = (i64::from(y) + i64::from(h)).min(i64::from(screen.1));
        if left >= right || top >= bottom {
            continue;
        }
        let viewport = [
            left as f32,
            top as f32,
            (right - left) as f32,
            (bottom - top) as f32,
        ];
        if !viewports.contains(&viewport) {
            viewports.push(viewport);
        }
    }
    // Preserve the single-framebuffer fallback only when output geometry is
    // not available. Known outputs outside this framebuffer are not a reason
    // to draw chrome in a desktop hole.
    if !has_monitors && screen.0 > 0 && screen.1 > 0 {
        viewports.push([0.0, 0.0, screen.0 as f32, screen.1 as f32]);
    }
    viewports
}

/// A deterministic REC-first placement set. Later intersecting chips are
/// omitted on partially overlapping outputs; this is not a packing solver.
#[derive(Debug, Default)]
pub(crate) struct RecordingIndicatorPlacements {
    pub(crate) recording: Vec<RecordingIndicatorLayout>,
    pub(crate) microphone: Vec<RecordingIndicatorLayout>,
}

impl RecordingIndicatorPlacements {
    pub(crate) fn chips(&self) -> impl Iterator<Item = [f32; 4]> + '_ {
        self.recording
            .iter()
            .chain(&self.microphone)
            .map(|layout| layout.chip)
    }
}

fn rects_overlap(a: [f32; 4], b: [f32; 4]) -> bool {
    a[0] < b[0] + b[2] && b[0] < a[0] + a[2] && a[1] < b[1] + b[3] && b[1] < a[1] + a[3]
}

pub(crate) fn recording_indicator_placements(
    viewports: &[[f32; 4]],
    recording_text: Option<(f32, f32)>,
    microphone_text: Option<(f32, f32)>,
) -> RecordingIndicatorPlacements {
    let mut placed = RecordingIndicatorPlacements::default();
    if let Some((tw, th)) = recording_text {
        for &[x, y, w, h] in viewports {
            let layout = recording_indicator_layout(w, h, tw, th).translated(x, y);
            if !placed.chips().any(|chip| rects_overlap(chip, layout.chip)) {
                placed.recording.push(layout);
            }
        }
    }
    if let Some((tw, th)) = microphone_text {
        for &[x, y, w, h] in viewports {
            let rec_h =
                recording_text.map(|(rw, rh)| recording_indicator_layout(w, h, rw, rh).chip[3]);
            let layout = mic_indicator_layout(w, h, tw, th, rec_h);
            if !mic_indicator_fits(&layout, h, rec_h) {
                continue;
            }
            let layout = layout.translated(x, y);
            if !placed.chips().any(|chip| rects_overlap(chip, layout.chip)) {
                placed.microphone.push(layout);
            }
        }
    }
    placed
}

/// Test the actual tooltip candidate against the final chip set, including
/// neighboring outputs. Try the existing alternative placement; hide the
/// tooltip if neither candidate is clear instead of covering a live cue.
pub(crate) fn tooltip_avoiding_recording_chips(
    placed: &RecordingIndicatorPlacements,
    mut position: impl FnMut(Option<[f32; 4]>) -> Option<[f32; 4]>,
) -> Option<[f32; 4]> {
    let candidate = position(None)?;
    let collision = placed.chips().find(|&chip| rects_overlap(candidate, chip));
    let Some(collision) = collision else {
        return Some(candidate);
    };
    let alternative = position(Some(collision))?;
    (!placed.chips().any(|chip| rects_overlap(alternative, chip))).then_some(alternative)
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
    let screen_w = screen_w.max(0.0);
    let screen_h = screen_h.max(0.0);
    let text_w = if text_w.is_finite() {
        text_w.max(0.0)
    } else {
        0.0
    };
    let text_h = if text_h.is_finite() {
        text_h.max(0.0)
    } else {
        0.0
    };
    let natural_w = CHIP_PAD_X + CHIP_DOT + CHIP_DOT_GAP + text_w + CHIP_PAD_X;
    let natural_h = (text_h + 2.0 * CHIP_PAD_Y).max(CHIP_DOT + 2.0 * CHIP_PAD_Y);
    let chip_w = natural_w.min(screen_w);
    let chip_h = natural_h.min(screen_h);
    let pad_x = rec_pad_x(chip_w);
    let pad_y = rec_pad_y(chip_h);
    let margin_x = rec_margin(screen_w);
    let margin_y = rec_margin(screen_h);
    let x = (screen_w - margin_x - chip_w).max(0.0);
    let y = (screen_h - margin_y - chip_h).max(0.0);
    let inner_w = (chip_w - 2.0 * pad_x).max(0.0);
    let dot = rec_dot(chip_h);
    let gap = rec_dot_gap(chip_w);
    let (dot_w, text_draw_w, text_x_off) = if inner_w >= dot + gap {
        (
            dot.min(chip_h),
            (inner_w - dot - gap).min(text_w),
            pad_x + dot + gap,
        )
    } else if inner_w >= dot * 0.5 {
        (inner_w.min(chip_h).min(dot), 0.0, pad_x)
    } else {
        (0.0, inner_w.min(text_w), pad_x)
    };
    let text_draw_h = text_h.min((chip_h - 2.0 * pad_y).max(0.0)).min(chip_h);
    RecordingIndicatorLayout {
        chip: [x, y, chip_w, chip_h],
        dot: [x + pad_x, y + (chip_h - dot_w) / 2.0, dot_w, dot_w],
        text: [
            x + text_x_off,
            y + (chip_h - text_draw_h) / 2.0,
            text_draw_w,
            text_draw_h,
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
        // This raw layout clamps to the output. The placement policy below
        // suppresses it if a short output cannot keep MIC separate from REC.
        let gap = if screen_h.is_finite() && screen_h > 0.0 && screen_h < 8.0 * CHIP_STACK_GAP {
            CHIP_STACK_GAP.min(screen_h * 0.04).max(2.0)
        } else {
            CHIP_STACK_GAP
        };
        let lift = (rec_chip_h + gap).min(layout.chip[1]);
        layout.chip[1] -= lift;
        layout.dot[1] -= lift;
        layout.text[1] -= lift;
    }
    layout
}

/// Keep REC readable when an output is too short for both chips. The MIC
/// cue remains available on other outputs, or alone when REC is inactive.
pub(crate) fn mic_indicator_fits(
    layout: &RecordingIndicatorLayout,
    screen_h: f32,
    rec_chip_h: Option<f32>,
) -> bool {
    rec_chip_h.is_none_or(|height| {
        let rec_top = (screen_h - rec_margin(screen_h) - height).max(0.0);
        layout.chip[1] + layout.chip[3] <= rec_top
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indicators_stay_on_each_real_output_in_an_l_shaped_desktop() {
        let outputs = recording_indicator_viewports(
            (3840, 2160),
            [(0, 0, 1920, 2160), (1920, 0, 1920, 1080)],
        );
        assert_eq!(
            outputs,
            vec![[0.0, 0.0, 1920.0, 2160.0], [1920.0, 0.0, 1920.0, 1080.0]]
        );
        let old = recording_indicator_layout(3840.0, 2160.0, 88.0, 14.0).chip;
        assert!(
            old[0] >= 1920.0 && old[1] >= 1080.0,
            "the root corner is a screen hole"
        );
        for [x, y, w, h] in outputs {
            let rec = recording_indicator_layout(w, h, 88.0, 14.0).translated(x, y);
            let mic = mic_indicator_layout(w, h, 28.0, 14.0, Some(rec.chip[3])).translated(x, y);
            for layout in [rec, mic] {
                for rect in [layout.chip, layout.dot, layout.text] {
                    assert!(rect[0] >= x && rect[1] >= y);
                    assert!(rect[0] + rect[2] <= x + w && rect[1] + rect[3] <= y + h);
                }
            }
            assert!(mic.chip[1] + mic.chip[3] <= rec.chip[1]);
        }
    }

    #[test]
    fn indicator_outputs_clip_negative_origins_and_deduplicate_mirrors() {
        assert_eq!(
            recording_indicator_viewports(
                (1920, 1080),
                [
                    (-100, -50, 300, 250),
                    (0, 0, 200, 200),
                    (1800, 1000, 500, 500),
                    (i32::MIN, i32::MIN, 1, 1),
                    (i32::MAX, 0, u32::MAX, 10),
                    (0, 0, 0, 10),
                ]
            ),
            vec![[0.0, 0.0, 200.0, 200.0], [1800.0, 1000.0, 120.0, 80.0]]
        );
        assert_eq!(
            recording_indicator_viewports((800, 600), []),
            vec![[0.0, 0.0, 800.0, 600.0]]
        );
        assert!(recording_indicator_viewports((800, 600), [(-200, -200, 100, 100)]).is_empty());
        assert!(recording_indicator_viewports((0, 600), []).is_empty());
    }

    #[test]
    fn tiny_outputs_prioritize_rec_without_overlapping_mic() {
        for height in [1.0, 8.0, 24.0, 50.0, 64.0, 1080.0] {
            let rec = recording_indicator_layout(200.0, height, 60.0, 19.0);
            let mic = mic_indicator_layout(200.0, height, 48.0, 19.0, Some(rec.chip[3]));
            assert_eq!(
                mic_indicator_fits(&mic, height, Some(rec.chip[3])),
                mic.chip[1] + mic.chip[3] <= rec.chip[1]
            );
            assert!(mic_indicator_fits(&mic, height, None));
        }
    }

    #[test]
    fn tiny_output_indicator_pixels_stay_inside_the_visible_viewport() {
        for (w, h) in [(1.0, 1.0), (8.0, 12.0), (32.0, 24.0), (80.0, 50.0)] {
            let rec = recording_indicator_layout(w, h, 200.0, 40.0).translated(100.0, 70.0);
            let mic =
                mic_indicator_layout(w, h, 100.0, 40.0, Some(rec.chip[3])).translated(100.0, 70.0);
            for layout in [rec, mic] {
                for rect in [layout.chip, layout.dot, layout.text] {
                    assert!(rect.iter().all(|value| value.is_finite()));
                    if rect[2] > 0.0 && rect[3] > 0.0 {
                        assert!(rect[0] >= 100.0 && rect[1] >= 70.0);
                        assert!(rect[0] + rect[2] <= 100.0 + w + 0.001);
                        assert!(rect[1] + rect[3] <= 70.0 + h + 0.001);
                    }
                }
            }
        }
    }

    #[test]
    fn overlapping_outputs_place_rec_first_without_duplicate_pixels() {
        let views = recording_indicator_viewports(
            (4000, 2200),
            [
                (0, 0, 1920, 1080),
                (20, 10, 1920, 1080),
                (0, 0, 1920, 1080),
                (2000, 0, 1920, 1080),
            ],
        );
        let placed = recording_indicator_placements(&views, Some((60.0, 19.0)), Some((48.0, 19.0)));
        assert_eq!(placed.recording.len(), 2);
        assert!(!placed.microphone.is_empty());
        let chips: Vec<_> = placed.chips().collect();
        for (i, a) in chips.iter().enumerate() {
            for b in &chips[i + 1..] {
                assert!(!rects_overlap(*a, *b));
            }
        }
        let plain = recording_indicator_placements(
            &[views[0], views[2]],
            Some((60.0, 19.0)),
            Some((48.0, 19.0)),
        );
        assert_eq!(plain.recording.len(), 2);
        assert_eq!(plain.microphone.len(), 2);
        assert_eq!(
            plain.recording[0],
            recording_indicator_layout(1920.0, 1080.0, 60.0, 19.0)
        );
    }

    #[test]
    fn tooltip_avoidance_checks_all_final_chips_and_hides_if_blocked() {
        let views = [[0.0, 0.0, 1920.0, 2160.0], [1920.0, 0.0, 1920.0, 1080.0]];
        let placed = recording_indicator_placements(&views, Some((60.0, 19.0)), Some((48.0, 19.0)));
        let hole = [3700.0, 2000.0, 100.0, 30.0];
        assert_eq!(
            tooltip_avoiding_recording_chips(&placed, |_| Some(hole)),
            Some(hole)
        );
        let blocked = placed.recording[1].chip;
        assert_eq!(
            tooltip_avoiding_recording_chips(&placed, |avoid| Some(if avoid.is_none() {
                blocked
            } else {
                hole
            })),
            Some(hole)
        );
        assert_eq!(
            tooltip_avoiding_recording_chips(&placed, |_| Some(blocked)),
            None
        );
        let other = placed.recording[0].chip;
        assert_eq!(
            tooltip_avoiding_recording_chips(&placed, |avoid| Some(if avoid.is_none() {
                blocked
            } else {
                other
            })),
            None
        );
    }

    #[test]
    fn both_renderers_reuse_cached_labels_across_actual_output_layouts() {
        for source in [
            include_str!("../x11/compositor/render.rs"),
            include_str!("../wayland_udev/compositor/render.rs"),
        ] {
            for name in ["render_recording_indicator", "render_mic_indicator"] {
                let signature = format!("fn {name}(");
                let body = source.split_once(&signature).unwrap().1;
                let body = body.split_once("        drawn_height\n    }").unwrap().0;
                let cache = body.find("self.update_").unwrap();
                let outputs = body
                    .find("indicator::recording_indicator_viewports(")
                    .unwrap();
                let draw_loop = body.find("for layout in placements.").unwrap();
                assert!(
                    cache < outputs && outputs < draw_loop,
                    "one cached label is shared by every output"
                );
                assert!(body.contains("indicator::recording_indicator_placements("));
            }
        }
    }

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

        // A screen smaller than the chip still shows it, pinned to the origin
        // and clipped to the pixels that exist.
        let tiny = recording_indicator_layout(10.0, 8.0, 60.0, 19.0);
        assert_eq!(tiny.chip[0], 0.0);
        assert_eq!(tiny.chip[1], 0.0);
        assert!(tiny.chip[2] <= 10.0);
        assert!(tiny.chip[3] <= 8.0);
        assert!(tiny.text[2] <= tiny.chip[2]);
        assert!(tiny.dot[2] <= tiny.chip[2]);
        let cramped = recording_indicator_layout(36.0, 28.0, 60.0, 19.0);
        assert!(cramped.text[2] + cramped.dot[2] <= cramped.chip[2] + 1e-3);
        assert!(cramped.text[0] >= cramped.chip[0]);
        assert!(cramped.dot[0] >= cramped.chip[0]);
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
        assert_eq!(mic.dot[1], (mic.chip[3] - mic.dot[3]) / 2.0);
        let squat = recording_indicator_layout(80.0, 16.0, 40.0, 8.0);
        assert!(squat.dot[2] < CHIP_DOT);
        assert!(squat.dot[2] >= 3.0);
        assert!(squat.dot[2] <= squat.chip[3] + 0.01);
        let rec = recording_indicator_layout(1920.0, 64.0, 60.0, 19.0);
        let mic = mic_indicator_layout(1920.0, 64.0, 48.0, 19.0, Some(rec.chip[3]));
        let gap = rec.chip[1] - (mic.chip[1] + mic.chip[3]);
        assert!(gap < CHIP_STACK_GAP);
        assert!(gap > 0.0 || mic.chip[1] == 0.0);
    }

    #[test]
    fn the_live_dot_pulses_but_never_vanishes() {
        let a0 = dot_alpha(Duration::ZERO);
        assert!(a0 > 0.0);
        let mut min = a0;
        let mut max = a0;
        for ms in (0..1000).step_by(50) {
            let a = dot_alpha(Duration::from_millis(ms));
            min = min.min(a);
            max = max.max(a);
            assert!(a >= dot_color()[3] * DOT_ALPHA_FLOOR - 1e-4);
            assert!(a <= dot_color()[3] + 1e-4);
        }
        assert!(max > min, "the pulse has a range");
    }

    #[test]
    fn recording_placements_cover_only_the_drawn_pills() {
        let view = [[0.0, 0.0, 1920.0, 1080.0]];
        assert_eq!(
            recording_indicator_placements(&view, None, None)
                .chips()
                .count(),
            0
        );
        let rec = recording_indicator_placements(&view, Some((88.0, 14.0)), None);
        let both = recording_indicator_placements(&view, Some((88.0, 14.0)), Some((28.0, 14.0)));
        assert_eq!(rec.chips().count(), 1);
        assert_eq!(both.chips().count(), 2);
        assert_eq!(rec.recording, both.recording);
        let cramped = recording_indicator_placements(
            &[[0.0, 0.0, 80.0, 24.0]],
            Some((88.0, 14.0)),
            Some((28.0, 14.0)),
        );
        assert_eq!(cramped.recording.len(), 1);
        assert!(cramped.microphone.is_empty());
        for rect in cramped.chips() {
            assert!(rect[0] >= 0.0 && rect[1] >= 0.0);
            assert!(rect[0] + rect[2] <= 80.0 && rect[1] + rect[3] <= 24.0);
        }
    }
}
