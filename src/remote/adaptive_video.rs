//! Local hardware-video adaptation. Observation windows use the host's clock.

use std::io;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use super::messages::ViewerFrameFeedback;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Policy {
    Text,
    Desktop,
    Motion,
}

impl Policy {
    pub(super) fn from_environment() -> io::Result<Option<Self>> {
        match std::env::var_os("JWM_REMOTE_ADAPT_VIDEO").as_deref() {
            None => Ok(None),
            Some(value) if value == "text" => Ok(Some(Self::Text)),
            Some(value) if value == "desktop" => Ok(Some(Self::Desktop)),
            Some(value) if value == "motion" => Ok(Some(Self::Motion)),
            Some(_) => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "JWM_REMOTE_ADAPT_VIDEO must be text, desktop or motion",
            )),
        }
    }

    pub(super) fn resolution_from_environment(self) -> io::Result<bool> {
        match std::env::var_os("JWM_REMOTE_ADAPT_RESOLUTION").as_deref() {
            None => Ok(false),
            Some(value) if value == "0" => Ok(false),
            Some(value) if value == "1" => Ok(self != Self::Text),
            Some(_) => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "JWM_REMOTE_ADAPT_RESOLUTION must be 0 or 1",
            )),
        }
    }

    fn fps_floor(self) -> u16 {
        match self {
            Self::Text => 8,
            Self::Desktop => 12,
            Self::Motion => 24,
        }
    }

    fn bitrate_floor(self) -> u16 {
        match self {
            Self::Text => 70,
            Self::Desktop => 55,
            Self::Motion => 45,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Rate {
    pub(super) fps: u16,
    pub(super) bitrate_percent: u16,
}

impl Rate {
    pub(super) fn fixed(fps: u16) -> Self {
        Self {
            fps,
            bitrate_percent: 100,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Resolution {
    #[default]
    Full,
    ThreeQuarters,
    Half,
}

impl Resolution {
    pub(super) fn percent(self) -> u16 {
        match self {
            Self::Full => 100,
            Self::ThreeQuarters => 75,
            Self::Half => 50,
        }
    }

    fn larger(self) -> Option<Self> {
        match self {
            Self::Full => None,
            Self::ThreeQuarters => Some(Self::Full),
            Self::Half => Some(Self::ThreeQuarters),
        }
    }

    fn smaller(self, base: (u16, u16)) -> Option<Self> {
        let next = match self {
            Self::Full => Self::ThreeQuarters,
            Self::ThreeQuarters => Self::Half,
            Self::Half => return None,
        };
        (next.dimensions(base) != self.dimensions(base)).then_some(next)
    }

    pub(super) fn dimensions(self, base: (u16, u16)) -> (u16, u16) {
        if self == Self::Full {
            return base;
        }
        let scale = |axis| (u32::from(axis) * u32::from(self.percent()) / 100) as u16;
        let reduced = (scale(base.0), scale(base.1));
        // Avoid introducing encoder sizes below the conservative tier floor.
        // A smaller source or viewport still works at its ordinary size.
        if reduced.0.max(reduced.1) >= 320 && reduced.0.min(reduced.1) >= 180 {
            return reduced;
        }
        match self.larger() {
            Some(larger) => larger.dimensions(base),
            None => base,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Decision {
    Rate(Rate),
    Resolution(Resolution),
}

pub(super) struct RateTarget(AtomicU32);

impl RateTarget {
    pub(super) fn new(rate: Rate) -> Self {
        Self(AtomicU32::new(Self::pack(rate)))
    }

    fn pack(rate: Rate) -> u32 {
        u32::from(rate.fps) << 16 | u32::from(rate.bitrate_percent)
    }

    pub(super) fn store(&self, rate: Rate) {
        self.0.store(Self::pack(rate), Ordering::Release);
    }

    pub(super) fn load(&self) -> Rate {
        let rate = self.0.load(Ordering::Acquire);
        Rate {
            fps: (rate >> 16) as u16,
            bitrate_percent: rate as u16,
        }
    }
}

pub(super) struct Observation {
    pub(super) captured: u64,
    pub(super) sent: u64,
    pub(super) backpressure_skips: u64,
    pub(super) capture_ms: f64,
    pub(super) encode_ms: f64,
    pub(super) socket_write_ms: f64,
    pub(super) credit_wait_ms: f64,
    pub(super) feedback_expected: bool,
    pub(super) viewer: Option<ViewerLoad>,
}

#[derive(Clone, Copy)]
pub(super) struct ViewerLoad {
    pub(super) samples: u64,
    pub(super) decode_ms: f64,
    pub(super) queue_ms: f64,
    pub(super) presentation_ms: f64,
}

#[derive(Default)]
pub(super) struct FeedbackWindow {
    scope: (u64, u64),
    samples: u64,
    elapsed: ViewerFrameFeedback,
}

impl FeedbackWindow {
    pub(super) fn record(&mut self, scope: (u64, u64), feedback: ViewerFrameFeedback) {
        if self.scope != scope {
            *self = Self {
                scope,
                ..Self::default()
            };
        }
        self.samples = self.samples.saturating_add(1);
        self.elapsed.decode = self.elapsed.decode.saturating_add(feedback.decode);
        self.elapsed.presentation_queue = self
            .elapsed
            .presentation_queue
            .saturating_add(feedback.presentation_queue);
        self.elapsed.presentation = self
            .elapsed
            .presentation
            .saturating_add(feedback.presentation);
    }

    pub(super) fn take(&mut self, scope: (u64, u64)) -> Option<ViewerLoad> {
        let window = std::mem::take(self);
        if window.scope != scope || window.samples < 12 {
            return None;
        }
        let mean = |elapsed: Duration| elapsed.as_secs_f64() * 1000.0 / window.samples as f64;
        Some(ViewerLoad {
            samples: window.samples,
            decode_ms: mean(window.elapsed.decode),
            queue_ms: mean(window.elapsed.presentation_queue),
            presentation_ms: mean(window.elapsed.presentation),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Pressure {
    Capture,
    Encode,
    Write,
    Viewer,
    ViewerRecovery,
    Credit,
    Clear,
}

impl Observation {
    fn pressure(&self, frame_period_ms: f64, viewer_can_restore_size: bool) -> Option<Pressure> {
        if self.captured < 12 || self.sent < 12 {
            return None;
        }
        if self.socket_write_ms > frame_period_ms * 0.45 {
            return Some(Pressure::Write);
        }
        if self.capture_ms > frame_period_ms * 0.7 {
            return Some(Pressure::Capture);
        }
        if self.encode_ms > frame_period_ms * 0.7 {
            return Some(Pressure::Encode);
        }
        let clear_stage_ms = frame_period_ms * 0.45;
        let host_has_headroom = self.socket_write_ms < frame_period_ms * 0.2
            && self.capture_ms < clear_stage_ms
            && self.encode_ms < clear_stage_ms;
        // CPU upscaling can keep presentation slow after decode has recovered.
        // Restore only a size lowered for decode, and keep the slower rate.
        if viewer_can_restore_size
            && host_has_headroom
            && self.viewer.is_some_and(|viewer| {
                viewer.decode_ms < clear_stage_ms
                    && viewer.queue_ms.max(viewer.presentation_ms) >= clear_stage_ms
            })
        {
            return Some(Pressure::ViewerRecovery);
        }
        let viewer_peak = self.viewer.map(|viewer| {
            viewer
                .decode_ms
                .max(viewer.queue_ms)
                .max(viewer.presentation_ms)
        });
        if viewer_peak.is_some_and(|peak| peak > frame_period_ms * 0.7) {
            return Some(Pressure::Viewer);
        }
        if self.credit_wait_ms > frame_period_ms * 0.55
            || self.backpressure_skips > self.captured / 4
        {
            return Some(Pressure::Credit);
        }
        (host_has_headroom
            && self.credit_wait_ms < frame_period_ms * 0.2
            && self.backpressure_skips <= self.captured / 10
            && (!self.feedback_expected || viewer_peak.is_some_and(|peak| peak < clear_stage_ms)))
        .then_some(Pressure::Clear)
    }
}

pub(super) struct Governor {
    policy: Policy,
    requested_fps: u16,
    rate: Rate,
    resolution: Resolution,
    // Viewer recovery may undo viewer-driven reductions up to this tier.
    host_resolution_floor: Resolution,
    // Most reduced tier the viewer decoder may request in this scope.
    viewer_resolution_floor: Resolution,
    previous: Option<Pressure>,
    windows: u8,
    frozen: u8,
}

impl Governor {
    pub(super) fn new(policy: Policy, requested_fps: u16) -> Self {
        Self {
            policy,
            requested_fps,
            rate: Rate::fixed(requested_fps),
            resolution: Resolution::Full,
            host_resolution_floor: Resolution::Full,
            viewer_resolution_floor: Resolution::Half,
            previous: None,
            windows: 0,
            frozen: 2,
        }
    }

    /// Wait for fresh performance windows after changing sources, limits or rates.
    pub(super) fn freeze(&mut self) {
        self.previous = None;
        self.windows = 0;
        self.frozen = 2;
    }

    pub(super) fn reset_resolution(&mut self) {
        self.resolution = Resolution::Full;
        self.host_resolution_floor = Resolution::Full;
        self.viewer_resolution_floor = Resolution::Half;
        self.freeze();
    }

    pub(super) fn observe(
        &mut self,
        observation: Observation,
        resolution_base: Option<(u16, u16)>,
    ) -> Option<(Pressure, Decision)> {
        if self.frozen != 0 {
            self.frozen -= 1;
            return None;
        }
        let period_ms = 1000.0 / f64::from(self.rate.fps);
        let pressure =
            observation.pressure(period_ms, self.resolution > self.host_resolution_floor);
        self.windows = if self.previous == pressure {
            self.windows.saturating_add(1)
        } else {
            1
        };
        self.previous = pressure;
        let pressure = pressure?;
        let old_rate = self.rate;
        let old_resolution = self.resolution;
        match pressure {
            Pressure::Clear if self.windows >= 4 => {
                if self.resolution == Resolution::Full {
                    self.viewer_resolution_floor = Resolution::Half;
                }
                if self.rate.bitrate_percent < 100 {
                    self.rate.bitrate_percent = (self.rate.bitrate_percent + 10).min(100);
                } else if let Some(larger) = self.resolution.larger() {
                    self.resolution = larger;
                } else {
                    self.rate.fps = (self.rate.fps + 4).min(self.requested_fps);
                }
            }
            Pressure::ViewerRecovery if self.windows >= 4 => {
                if let Some(larger) = self.resolution.larger() {
                    self.resolution = larger;
                    // Avoid bouncing through the same resize when enlargement
                    // costs undo the reduction. Re-arm after clear full-size
                    // windows or a new source or viewer bound.
                    self.viewer_resolution_floor = larger;
                }
            }
            Pressure::Write if self.windows >= 2 => {
                let floor = self.policy.bitrate_floor();
                if self.rate.bitrate_percent > floor {
                    self.rate.bitrate_percent =
                        self.rate.bitrate_percent.saturating_sub(15).max(floor);
                } else {
                    self.lower_fps();
                }
            }
            Pressure::Capture | Pressure::Encode | Pressure::Viewer | Pressure::Credit
                if self.windows >= 2 =>
            {
                self.lower_fps();
                let host_pressure = matches!(pressure, Pressure::Capture | Pressure::Encode);
                let decode_pressure = pressure == Pressure::Viewer
                    && observation
                        .viewer
                        .is_some_and(|viewer| viewer.decode_ms > period_ms * 0.7);
                if self.rate == old_rate
                    && self.policy != Policy::Text
                    && let Some(next) =
                        resolution_base.and_then(|base| self.resolution.smaller(base))
                    && (host_pressure || (decode_pressure && next <= self.viewer_resolution_floor))
                {
                    self.resolution = next;
                    if host_pressure {
                        self.host_resolution_floor = next;
                    }
                }
            }
            _ => {}
        }
        let decision = if self.rate != old_rate {
            Decision::Rate(self.rate)
        } else if self.resolution != old_resolution {
            self.host_resolution_floor = self.host_resolution_floor.min(self.resolution);
            Decision::Resolution(self.resolution)
        } else {
            return None;
        };
        self.freeze();
        Some((pressure, decision))
    }

    fn lower_fps(&mut self) {
        let floor = self.requested_fps.min(self.policy.fps_floor());
        self.rate.fps = (self.rate.fps * 4 / 5).max(floor);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observation(capture_ms: f64, socket: f64) -> Observation {
        Observation {
            captured: 60,
            sent: 60,
            backpressure_skips: 0,
            capture_ms,
            encode_ms: 2.0,
            socket_write_ms: socket,
            credit_wait_ms: 0.0,
            feedback_expected: false,
            viewer: None,
        }
    }

    #[test]
    fn sustained_write_pressure_changes_bitrate_without_changing_fps() {
        let mut governor = Governor::new(Policy::Desktop, 30);
        for _ in 0..3 {
            assert!(governor.observe(observation(2.0, 19.0), None).is_none());
        }
        assert_eq!(
            governor.observe(observation(2.0, 19.0), None),
            Some((
                Pressure::Write,
                Decision::Rate(Rate {
                    fps: 30,
                    bitrate_percent: 85,
                }),
            ))
        );
        for _ in 0..5 {
            assert!(governor.observe(observation(2.0, 3.0), None).is_none());
        }
        assert_eq!(
            governor.observe(observation(2.0, 3.0), None),
            Some((
                Pressure::Clear,
                Decision::Rate(Rate {
                    fps: 30,
                    bitrate_percent: 95,
                }),
            ))
        );
    }

    #[test]
    fn a_source_change_restarts_hysteresis_and_capture_pressure_reduces_fps() {
        let mut governor = Governor::new(Policy::Text, 30);
        for _ in 0..3 {
            assert!(governor.observe(observation(30.0, 1.0), None).is_none());
        }
        governor.freeze();
        for _ in 0..3 {
            assert!(governor.observe(observation(30.0, 1.0), None).is_none());
        }
        assert_eq!(
            governor.observe(observation(30.0, 1.0), None),
            Some((Pressure::Capture, Decision::Rate(Rate::fixed(24))))
        );
    }

    #[test]
    fn viewer_pressure_reduces_fps_and_recovery_needs_fresh_viewer_feedback() {
        let mut governor = Governor::new(Policy::Desktop, 30);
        let viewer = |queue_ms| Observation {
            feedback_expected: true,
            viewer: Some(ViewerLoad {
                samples: 60,
                decode_ms: 3.0,
                queue_ms,
                presentation_ms: 5.0,
            }),
            ..observation(2.0, 1.0)
        };
        for _ in 0..3 {
            assert!(governor.observe(viewer(30.0), None).is_none());
        }
        assert_eq!(
            governor.observe(viewer(30.0), None),
            Some((Pressure::Viewer, Decision::Rate(Rate::fixed(24))))
        );
        for _ in 0..8 {
            assert!(
                governor
                    .observe(
                        Observation {
                            feedback_expected: true,
                            viewer: None,
                            ..observation(2.0, 1.0)
                        },
                        None,
                    )
                    .is_none()
            );
        }
        for _ in 0..3 {
            assert!(governor.observe(viewer(1.0), None).is_none());
        }
        assert_eq!(
            governor.observe(viewer(1.0), None),
            Some((Pressure::Clear, Decision::Rate(Rate::fixed(28))))
        );
    }

    #[test]
    fn resolution_changes_one_tier_at_a_time_after_fps_reaches_its_floor() {
        let mut governor = Governor::new(Policy::Motion, 30);
        let base = Some((1280, 800));
        let actions = (0..12)
            .filter_map(|_| governor.observe(observation(45.0, 1.0), base))
            .collect::<Vec<_>>();
        assert_eq!(
            actions,
            [
                (Pressure::Capture, Decision::Rate(Rate::fixed(24))),
                (
                    Pressure::Capture,
                    Decision::Resolution(Resolution::ThreeQuarters)
                ),
                (Pressure::Capture, Decision::Resolution(Resolution::Half)),
            ]
        );

        let actions = (0..18)
            .filter_map(|_| governor.observe(observation(1.0, 1.0), base))
            .collect::<Vec<_>>();
        assert_eq!(
            actions,
            [
                (
                    Pressure::Clear,
                    Decision::Resolution(Resolution::ThreeQuarters)
                ),
                (Pressure::Clear, Decision::Resolution(Resolution::Full)),
                (Pressure::Clear, Decision::Rate(Rate::fixed(28))),
            ]
        );
    }

    #[test]
    fn viewer_decode_recovery_can_restore_size_while_cpu_upscale_is_slow() {
        let mut governor = Governor::new(Policy::Motion, 24);
        let base = Some((1280, 800));
        let viewer = |decode_ms, presentation_ms| Observation {
            feedback_expected: true,
            viewer: Some(ViewerLoad {
                samples: 60,
                decode_ms,
                queue_ms: 2.0,
                presentation_ms,
            }),
            ..observation(2.0, 1.0)
        };
        let down = (0..8)
            .filter_map(|_| governor.observe(viewer(50.0, 5.0), base))
            .collect::<Vec<_>>();
        assert_eq!(
            down,
            [
                (
                    Pressure::Viewer,
                    Decision::Resolution(Resolution::ThreeQuarters)
                ),
                (Pressure::Viewer, Decision::Resolution(Resolution::Half)),
            ]
        );
        let up = (0..12)
            .filter_map(|_| governor.observe(viewer(4.0, 42.0), base))
            .collect::<Vec<_>>();
        assert_eq!(
            up,
            [
                (
                    Pressure::ViewerRecovery,
                    Decision::Resolution(Resolution::ThreeQuarters)
                ),
                (
                    Pressure::ViewerRecovery,
                    Decision::Resolution(Resolution::Full)
                ),
            ]
        );
        for _ in 0..12 {
            assert!(governor.observe(viewer(50.0, 5.0), base).is_none());
        }
        for _ in 0..4 {
            assert!(governor.observe(viewer(5.0, 5.0), base).is_none());
        }
        assert!(governor.observe(viewer(50.0, 5.0), base).is_none());
        assert_eq!(
            governor.observe(viewer(50.0, 5.0), base),
            Some((
                Pressure::Viewer,
                Decision::Resolution(Resolution::ThreeQuarters)
            ))
        );
    }

    #[test]
    fn slow_presentation_cannot_reduce_size_or_undo_a_host_reduction() {
        let mut governor = Governor::new(Policy::Motion, 24);
        let base = Some((1280, 800));
        let slow_presentation = || Observation {
            feedback_expected: true,
            viewer: Some(ViewerLoad {
                samples: 60,
                decode_ms: 4.0,
                queue_ms: 2.0,
                presentation_ms: 42.0,
            }),
            ..observation(2.0, 1.0)
        };
        for _ in 0..10 {
            assert!(governor.observe(slow_presentation(), base).is_none());
        }
        assert!(governor.observe(observation(45.0, 1.0), base).is_none());
        assert_eq!(
            governor.observe(observation(45.0, 1.0), base),
            Some((
                Pressure::Capture,
                Decision::Resolution(Resolution::ThreeQuarters)
            ))
        );
        for _ in 0..10 {
            assert!(governor.observe(slow_presentation(), base).is_none());
        }
    }

    #[test]
    fn text_disabled_small_and_unattributed_pressure_keep_the_base_size() {
        let base = Some((1280, 800));
        let cases = [
            (Policy::Text, 8, base, 45.0, 1.0, 0.0),
            (Policy::Motion, 24, None, 45.0, 1.0, 0.0),
            (Policy::Motion, 24, Some((321, 199)), 45.0, 1.0, 0.0),
            (Policy::Motion, 24, base, 1.0, 45.0, 0.0),
            (Policy::Motion, 24, base, 1.0, 1.0, 45.0),
        ];
        for (policy, fps, base, capture, write, credit) in cases {
            let mut governor = Governor::new(policy, fps);
            for _ in 0..24 {
                let result = governor.observe(
                    Observation {
                        credit_wait_ms: credit,
                        ..observation(capture, write)
                    },
                    base,
                );
                assert!(!matches!(result, Some((_, Decision::Resolution(_)))));
            }
        }
    }

    #[test]
    fn new_scope_restores_base_resolution_and_restarts_hysteresis() {
        let mut governor = Governor::new(Policy::Desktop, 12);
        let base = Some((723, 1153));
        for _ in 0..8 {
            governor.observe(observation(75.0, 1.0), base);
        }
        assert_eq!(governor.resolution, Resolution::Half);
        governor.reset_resolution();
        assert_eq!(governor.resolution, Resolution::Full);
        assert_eq!(governor.rate, Rate::fixed(12));
        for _ in 0..3 {
            assert!(governor.observe(observation(75.0, 1.0), base).is_none());
        }
        assert_eq!(
            governor.observe(observation(75.0, 1.0), base),
            Some((
                Pressure::Capture,
                Decision::Resolution(Resolution::ThreeQuarters)
            ))
        );
    }

    #[test]
    fn resolution_tiers_fit_bounded_odd_portrait_and_landscape_rasters() {
        assert_eq!(Resolution::ThreeQuarters.dimensions((799, 601)), (599, 450));
        assert_eq!(Resolution::Half.dimensions((601, 799)), (300, 399));
        assert_eq!(Resolution::Half.dimensions((480, 320)), (360, 240));
        assert_eq!(Resolution::Half.dimensions((199, 321)), (199, 321));
    }

    #[test]
    fn feedback_windows_do_not_mix_sources_or_geometries() {
        let mut window = FeedbackWindow::default();
        let feedback = ViewerFrameFeedback {
            decode: Duration::from_millis(35),
            ..ViewerFrameFeedback::default()
        };
        for _ in 0..12 {
            window.record((1, 2), feedback);
        }
        assert!(window.take((2, 2)).is_none());
        for _ in 0..12 {
            window.record((2, 3), feedback);
        }
        for _ in 0..11 {
            window.record((2, 4), feedback);
        }
        assert!(window.take((2, 4)).is_none());
        for _ in 0..12 {
            window.record((2, 4), feedback);
        }
        let viewer = window.take((2, 4)).unwrap();
        assert_eq!(viewer.samples, 12);
        assert_eq!(viewer.decode_ms, 35.0);
        assert!(window.take((2, 4)).is_none());
    }
}
