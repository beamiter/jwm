//! Bounded, local-clock timing windows shared by the host and viewer.

use std::time::Duration;
#[cfg(feature = "remote-h264")]
use std::time::Instant;

pub(super) const QUANTILE_POLICY: &str =
    "nearest-rank, most recent 128 observations per stage per window";

#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize)]
pub struct TimingDistribution {
    pub samples: usize,
    pub p50_ms: Option<f64>,
    pub p95_ms: Option<f64>,
    pub p99_ms: Option<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize)]
pub struct StageTiming {
    /// Mean over all observations in this window, in milliseconds.
    pub mean: Option<f64>,
    pub observations: u64,
    /// Quantiles over the most recent 128 observations in this window.
    pub quantiles: TimingDistribution,
}

/// Exact nearest-rank quantiles of the most recent observations in a window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct StageSamples {
    micros: [u64; 128],
    count: usize,
    next: usize,
}

impl Default for StageSamples {
    fn default() -> Self {
        Self {
            micros: [0; 128],
            count: 0,
            next: 0,
        }
    }
}

impl StageSamples {
    #[cfg(test)]
    pub(super) fn count(&self) -> usize {
        self.count
    }

    pub(super) fn record(&mut self, elapsed: Duration) {
        self.micros[self.next] = elapsed.as_micros().min(u128::from(u64::MAX)) as u64;
        self.next = (self.next + 1) % self.micros.len();
        self.count = (self.count + 1).min(self.micros.len());
    }

    pub(super) fn json(self) -> serde_json::Value {
        serde_json::json!(self.distribution())
    }

    fn distribution(self) -> TimingDistribution {
        let mut values = self.micros;
        values[..self.count].sort_unstable();
        let percentile = |percent: usize| {
            (self.count != 0)
                .then(|| values[(self.count * percent).div_ceil(100) - 1] as f64 / 1000.0)
        };
        TimingDistribution {
            samples: self.count,
            p50_ms: percentile(50),
            p95_ms: percentile(95),
            p99_ms: percentile(99),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct StageStats {
    pub(super) elapsed: Duration,
    pub(super) observations: u64,
    samples: StageSamples,
}

impl StageStats {
    pub(super) fn record(&mut self, elapsed: Duration) {
        self.elapsed = self.elapsed.saturating_add(elapsed);
        self.observations = self.observations.saturating_add(1);
        self.samples.record(elapsed);
    }

    pub(super) fn json(&self) -> serde_json::Value {
        serde_json::json!(self.snapshot())
    }

    pub(super) fn snapshot(&self) -> StageTiming {
        StageTiming {
            mean: (self.observations > 0)
                .then(|| self.elapsed.as_secs_f64() * 1000.0 / self.observations as f64),
            observations: self.observations,
            quantiles: self.samples.distribution(),
        }
    }
}

#[cfg(feature = "remote-h264")]
#[derive(Clone, Copy)]
pub(super) enum NativeStage {
    VaSurfaceWait,
    VaFrameExport,
    RgbTargetAllocation,
    EglPrepareAndSubmit,
    EglFenceWait,
    #[cfg(feature = "remote-wayland")]
    WaylandDmabufImport,
    X11DmabufImport,
    X11PictureSetup,
    X11BackingPopulate,
    X11WindowSubmit,
    X11PrefetchRgbAllocation,
}

#[cfg(feature = "remote-h264")]
const NATIVE_STAGE_COUNT: usize = NativeStage::X11PrefetchRgbAllocation as usize + 1;

#[cfg(feature = "remote-h264")]
#[derive(Clone, Copy, Default)]
pub(super) struct NativeStageTimes([Option<Duration>; NATIVE_STAGE_COUNT]);

#[cfg(feature = "remote-h264")]
impl NativeStageTimes {
    pub(super) fn record(&mut self, stage: NativeStage, elapsed: Duration) {
        self.0[stage as usize] = Some(elapsed);
    }

    pub(super) fn start(&mut self, stage: NativeStage) -> NativeStageTimer<'_> {
        NativeStageTimer {
            elapsed: &mut self.0[stage as usize],
            started: Instant::now(),
        }
    }
}

#[cfg(feature = "remote-h264")]
pub(super) struct NativeStageTimer<'a> {
    elapsed: &'a mut Option<Duration>,
    started: Instant,
}

#[cfg(feature = "remote-h264")]
impl Drop for NativeStageTimer<'_> {
    fn drop(&mut self) {
        // A resize can retry the same submission. Count time spent in both
        // passes as one observation for each stage that actually ran.
        *self.elapsed = Some(
            self.elapsed
                .unwrap_or_default()
                .saturating_add(self.started.elapsed()),
        );
    }
}

#[cfg(feature = "remote-h264")]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct NativeStageStats([StageStats; NATIVE_STAGE_COUNT]);

#[cfg(feature = "remote-h264")]
impl NativeStageStats {
    pub(super) fn record(&mut self, times: NativeStageTimes) {
        for (stage, elapsed) in self.0.iter_mut().zip(times.0) {
            if let Some(elapsed) = elapsed {
                stage.record(elapsed);
            }
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.0.iter().all(|stage| stage.observations == 0)
    }

    pub(super) fn json(&self) -> Option<serde_json::Value> {
        use NativeStage::*;

        let names = [
            (VaSurfaceWait, "va_surface_wait_ms"),
            (VaFrameExport, "va_frame_export_ms"),
            (RgbTargetAllocation, "rgb_target_allocation_ms"),
            (EglPrepareAndSubmit, "egl_prepare_and_submit_ms"),
            (EglFenceWait, "egl_fence_wait_ms"),
            #[cfg(feature = "remote-wayland")]
            (WaylandDmabufImport, "wayland_dmabuf_import_ms"),
            (X11DmabufImport, "x11_dmabuf_import_ms"),
            (X11PictureSetup, "x11_picture_setup_ms"),
            (X11BackingPopulate, "x11_backing_populate_ms"),
            (X11WindowSubmit, "x11_window_submit_ms"),
            (X11PrefetchRgbAllocation, "x11_prefetch_rgb_allocation_ms"),
        ];
        let stages: serde_json::Map<_, _> = names
            .into_iter()
            .filter_map(|(stage, name)| {
                let stats = &self.0[stage as usize];
                (stats.observations > 0).then(|| (name.to_owned(), stats.json()))
            })
            .collect();
        (!stages.is_empty()).then_some(serde_json::Value::Object(stages))
    }
}

#[cfg(all(test, feature = "remote-h264"))]
#[test]
fn native_x11_stage_stats_omit_picture_setup_when_deferred() {
    let mut times = NativeStageTimes::default();
    times.0[NativeStage::VaFrameExport as usize] = Some(Duration::from_millis(3));
    times.0[NativeStage::X11DmabufImport as usize] = Some(Duration::from_millis(2));
    times.0[NativeStage::X11BackingPopulate as usize] = Some(Duration::from_millis(6));
    let mut window = NativeStageStats::default();
    assert!(window.is_empty());
    window.record(times);
    let report = window.json().unwrap();
    assert_eq!(report.as_object().unwrap().len(), 3);
    assert_eq!(report["x11_dmabuf_import_ms"]["observations"], 1);
    assert_eq!(report["x11_backing_populate_ms"]["mean"], 6.0);
}

#[cfg(all(test, feature = "remote-h264", feature = "remote-wayland"))]
#[test]
fn native_wayland_stage_stats_serialize_only_observed_scopes() {
    let mut window = NativeStageStats::default();
    assert!(window.is_empty());
    assert!(window.json().is_none());

    let mut times = NativeStageTimes::default();
    times.0[NativeStage::VaSurfaceWait as usize] = Some(Duration::from_millis(4));
    times.0[NativeStage::WaylandDmabufImport as usize] = Some(Duration::ZERO);
    window.record(times);
    assert!(!window.is_empty());

    let json = window.json().unwrap();
    assert_eq!(json.as_object().unwrap().len(), 2);
    assert_eq!(json["va_surface_wait_ms"]["mean"], 4.0);
    assert_eq!(json["va_surface_wait_ms"]["quantiles"]["p95_ms"], 4.0);
    assert_eq!(json["wayland_dmabuf_import_ms"]["observations"], 1);
    assert_eq!(json["wayland_dmabuf_import_ms"]["mean"], 0.0);
}
