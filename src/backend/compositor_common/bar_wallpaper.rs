//! Wallpaper-only bar contrast measurements. Coordinates are the compositor's
//! top-left global canvas (logical pixels on Wayland, root pixels on X11).
//! Never read outside freshly repaired wallpaper: retained pixels may be windows.
use std::collections::BTreeMap;
use std::sync::Mutex;
#[cfg(not(test))]
use std::sync::OnceLock;
use std::time::{Duration, Instant};

const INTERVAL: Duration = Duration::from_secs(1);
const MAX_MONITORS: usize = 32;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Rect {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}
struct Entry {
    rect: Rect,
    pending: bool,
    attempt: Option<Instant>,
    sampled: Option<Instant>,
    unavailable: bool,
    value: Option<u8>,
    smoothed: f32,
    bright: Option<bool>,
}
#[derive(Default)]
struct Registry {
    entries: BTreeMap<i32, Entry>,
    changed: Vec<i32>,
    source: Option<u64>,
    dynamic: bool,
}
#[cfg(not(test))]
static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
#[cfg(not(test))]
fn registry() -> &'static Mutex<Registry> {
    REGISTRY.get_or_init(Mutex::default)
}

// Each WM test represents a separate compositor lifetime. Sharing the process
// singleton would let parallel publisher/scheduler fixtures contaminate one
// another. The tiny test-only allocation lives until the test process exits;
// production still uses exactly one synchronized registry across threads.
#[cfg(test)]
thread_local! {
    static TEST_REGISTRY: &'static Mutex<Registry> = Box::leak(Box::new(Mutex::default()));
}
#[cfg(test)]
fn registry() -> &'static Mutex<Registry> {
    TEST_REGISTRY.with(|registry| *registry)
}

/// Register the actual bar rectangle and read its cached linear luminance.
/// Missing/invalid/unsupported measurements deliberately return None.
pub(crate) fn register_and_read(id: i32, x: i32, y: i32, width: i32, height: i32) -> Option<u8> {
    let mut r = registry().lock().ok()?;
    if width <= 0 || height <= 0 {
        r.entries.remove(&id);
        return None;
    }
    let rect = Rect {
        x,
        y,
        w: width,
        h: height,
    };
    if !r.entries.contains_key(&id) && r.entries.len() >= MAX_MONITORS {
        return None;
    }
    let e = r.entries.entry(id).or_insert_with(|| Entry {
        rect,
        pending: true,
        attempt: None,
        sampled: None,
        unavailable: false,
        value: None,
        smoothed: 0.0,
        bright: None,
    });
    if e.rect != rect {
        *e = Entry {
            rect,
            pending: true,
            attempt: None,
            sampled: None,
            unavailable: false,
            value: None,
            smoothed: 0.0,
            bright: None,
        };
    }
    e.value
}
pub(crate) fn retain_monitors(ids: &[i32]) {
    if let Ok(mut r) = registry().lock() {
        r.entries.retain(|id, _| ids.contains(id));
        r.changed.retain(|id| ids.contains(id));
    }
}
pub(crate) fn take_changed_monitors() -> Vec<i32> {
    registry()
        .lock()
        .map(|mut r| std::mem::take(&mut r.changed))
        .unwrap_or_default()
}
/// Pending one-shot probes wake a static compositor. Successful static samples
/// remain cached; animation samples piggyback on its own frames, never wake it.
pub(crate) fn next_wakeup() -> Option<Duration> {
    let r = registry().lock().ok()?;
    if !r.changed.is_empty() {
        return Some(Duration::ZERO);
    }
    r.entries
        .values()
        .filter(|e| e.pending)
        .map(|e| {
            e.attempt
                .map_or(Duration::ZERO, |t| INTERVAL.saturating_sub(t.elapsed()))
        })
        .min()
}
pub(crate) fn needs_frame() -> bool {
    next_wakeup() == Some(Duration::ZERO)
}
/// Consume the attempt before any direct-scanout/fullscreen early return so an
/// unavailable wallpaper cannot cause a busy loop. A partial repair requeues it.
pub(crate) fn begin_frame() -> bool {
    let Ok(mut r) = registry().lock() else {
        return false;
    };
    let now = Instant::now();
    let mut due = false;
    for e in r.entries.values_mut() {
        if e.pending && e.attempt.is_none_or(|t| now.duration_since(t) >= INTERVAL) {
            e.attempt = Some(now);
            e.pending = false;
            due = true;
        }
    }
    due
}
/// One tightly bounded RGBA8 scanline at the actual bar's vertical center.
/// The whole readback, including pixels between the CPU samples, must belong
/// to fresh wallpaper. No retained client pixels may enter even the scratch row.
const MAX_SCANLINE_WIDTH: i32 = 16_384;
fn scanline(
    rect: Rect,
    screen: (u32, u32),
    repair: Option<(i32, i32, i32, i32)>,
) -> Option<(i32, i32, i32)> {
    let (sw, sh) = (i64::from(screen.0), i64::from(screen.1));
    let (x, y, w, h) = (
        i64::from(rect.x),
        i64::from(rect.y),
        i64::from(rect.w),
        i64::from(rect.h),
    );
    if x < 0
        || y < 0
        || w <= 0
        || w > i64::from(MAX_SCANLINE_WIDTH)
        || h <= 0
        || x + w > sw
        || y + h > sh
    {
        return None;
    }
    let py = sh - 1 - (y + h / 2);
    if let Some((rx, ry, rw, rh)) = repair {
        if x < i64::from(rx)
            || x + w > i64::from(rx) + i64::from(rw)
            || py < i64::from(ry)
            || py >= i64::from(ry) + i64::from(rh)
        {
            return None;
        }
    }
    Some((i32::try_from(x).ok()?, i32::try_from(py).ok()?, rect.w))
}
fn row_luminance(row: &[u8], width: i32) -> Option<f32> {
    if !(1..=MAX_SCANLINE_WIDTH).contains(&width) || row.len() != width as usize * 4 {
        return None;
    }
    let mut sum = 0.0;
    for col in 0..12 {
        let offset = ((2 * col + 1) * width as usize / 24).min(width as usize - 1) * 4;
        sum += luminance([
            row[offset],
            row[offset + 1],
            row[offset + 2],
            row[offset + 3],
        ]);
    }
    Some(sum / 12.0)
}
fn linear(v: u8) -> f32 {
    let c = f32::from(v) / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}
fn luminance(rgb: [u8; 4]) -> f32 {
    255.0 * (0.2126 * linear(rgb[0]) + 0.7152 * linear(rgb[1]) + 0.0722 * linear(rgb[2]))
}
fn stabilize(e: &mut Entry, measured: f32) -> u8 {
    e.smoothed = if e.value.is_none() {
        measured
    } else {
        0.5 * e.smoothed + 0.5 * measured
    };
    let bright = match e.bright {
        None => e.smoothed >= 46.0,
        Some(true) => e.smoothed > 38.0,
        Some(false) => e.smoothed >= 54.0,
    };
    e.bright = Some(bright);
    // Keep the consumer's 46 threshold stable throughout the hysteresis band.
    let value = e.smoothed.round().clamp(0.0, 255.0) as u8;
    if bright { value.max(46) } else { value.min(45) }
}
/// Avoid even GL state queries when a static cached sample is still valid.
pub(crate) fn sample_due(source: u64, dynamic: bool) -> bool {
    let Ok(r) = registry().lock() else {
        return false;
    };
    !r.entries.is_empty()
        && (r.source != Some(source)
            || r.dynamic != dynamic
            || r.entries.values().any(|e| {
                !e.unavailable
                    && (e.value.is_none() || e.pending || dynamic)
                    && e.sampled.is_none_or(|t| t.elapsed() >= INTERVAL)
            }))
}

/// Invoke only after ALL wallpaper passes, before any client/overlay pass.
/// `repair` is bottom-left GL damage; `eligible=false` means wallpaper was culled.
/// The callback reads one RGBA8 scanline (at most 65,536 bytes) in one GL call,
/// with safe pixel-pack state. Twelve spaced pixels are averaged on the CPU.
pub(crate) fn sample_frame(
    source: u64,
    dynamic: bool,
    screen: (u32, u32),
    repair: Option<(i32, i32, i32, i32)>,
    eligible: bool,
    mut read: impl FnMut(i32, i32, i32) -> Option<Vec<u8>>,
) {
    let Ok(mut r) = registry().lock() else {
        return;
    };
    let source_changed = r.source != Some(source) || r.dynamic != dynamic;
    r.source = Some(source);
    r.dynamic = dynamic;
    let mut changed = Vec::new();
    let now = Instant::now();
    for (&id, e) in &mut r.entries {
        if source_changed {
            if e.value.take().is_some() {
                changed.push(id);
            }
            e.bright = None;
            e.pending = true;
            e.attempt = None;
            e.sampled = None;
            e.unavailable = false;
        }
        if !eligible {
            e.pending = false;
            continue;
        }
        if e.unavailable || e.sampled.is_some_and(|t| now.duration_since(t) < INTERVAL) {
            continue;
        }
        if !e.pending && e.value.is_some() && !dynamic {
            continue;
        }
        let Some((x, y, width)) = scanline(e.rect, screen, repair) else {
            // Off-canvas/negative geometry cannot be mapped by shifting just
            // the probe: both renderers use a zero-origin projection. Settle
            // unavailable until the source or registration geometry changes.
            if scanline(e.rect, screen, None).is_none() {
                e.unavailable = true;
                e.pending = false;
            } else {
                e.pending = true; // valid bar, but current repair is too small
            }
            continue;
        };
        let measured = read(x, y, width).and_then(|row| row_luminance(&row, width));
        e.attempt = Some(now);
        let Some(measured) = measured else {
            e.sampled = Some(now);
            e.pending = false;
            e.unavailable = true;
            if e.value.take().is_some() {
                changed.push(id);
            }
            continue;
        };
        let value = stabilize(e, measured);
        if e.value != Some(value) {
            changed.push(id);
        }
        e.value = Some(value);
        e.sampled = Some(now);
        e.pending = false;
    }
    for id in changed {
        if !r.changed.contains(&id) {
            r.changed.push(id);
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn registry_caches_static_requeues_partial_and_bounds_failures() {
        *registry().lock().unwrap() = Registry::default();
        assert_eq!(register_and_read(1, 100, 10, 120, 20), None);
        assert!(begin_frame());
        let mut calls = 0;
        sample_frame(
            1,
            false,
            (400, 300),
            Some((0, 0, 10, 10)),
            true,
            |_, _, width| {
                calls += 1;
                Some(vec![255; width as usize * 4])
            },
        );
        assert_eq!(calls, 0);
        assert!(needs_frame());
        assert!(begin_frame());
        sample_frame(1, false, (400, 300), None, true, |_, _, width| {
            calls += 1;
            Some(vec![255; width as usize * 4])
        });
        assert_eq!(calls, 1);
        assert_eq!(register_and_read(1, 100, 10, 120, 20), Some(255));
        assert_eq!(take_changed_monitors(), vec![1]);
        assert_eq!(next_wakeup(), None);
        sample_frame(1, false, (400, 300), None, true, |_, _, _| {
            panic!("static cached")
        });
        // Dynamic attempts are bounded independently of compositor frame rate.
        sample_frame(2, true, (400, 300), None, true, |_, _, width| {
            calls += 1;
            Some([0, 0, 0, 255].repeat(width as usize))
        });
        sample_frame(2, true, (400, 300), None, true, |_, _, _| {
            panic!("rate limited")
        });
        assert_eq!(calls, 2);
        take_changed_monitors();
        // Failed reads settle unavailable, never retry every unrelated frame.
        sample_frame(3, true, (400, 300), None, true, |_, _, _| None);
        sample_frame(3, true, (400, 300), None, true, |_, _, _| {
            panic!("failed settled")
        });
        take_changed_monitors();
        assert_eq!(next_wakeup(), None);
        assert_eq!(register_and_read(1, -120, 10, 120, 20), None);
        begin_frame();
        sample_frame(3, false, (400, 300), None, true, |_, _, _| {
            panic!("off canvas")
        });
        take_changed_monitors();
        assert_eq!(next_wakeup(), None);
        retain_monitors(&[]);
        assert!(!sample_due(4, true));
        // Each output samples its own global rectangle; no primary-monitor leak.
        register_and_read(7, 0, 0, 100, 20);
        register_and_read(8, 200, 0, 100, 20);
        sample_frame(4, false, (400, 300), None, true, |x, _, width| {
            Some(if x < 100 {
                [0, 0, 0, 255].repeat(width as usize)
            } else {
                vec![255; width as usize * 4]
            })
        });
        assert_eq!(register_and_read(7, 0, 0, 100, 20), Some(0));
        assert_eq!(register_and_read(8, 200, 0, 100, 20), Some(255));
        retain_monitors(&[8]);
        assert_eq!(take_changed_monitors(), vec![8]);
        assert_eq!(registry().lock().unwrap().entries.len(), 1);
        retain_monitors(&[]);
        let mut read_calls = 0;
        let mut read_bytes = 0;
        register_and_read(9, 0, 0, MAX_SCANLINE_WIDTH, 1);
        sample_frame(5, false, (20000, 1), None, true, |x, y, width| {
            assert_eq!((x, y, width), (0, 0, MAX_SCANLINE_WIDTH));
            read_calls += 1;
            let row = vec![255; width as usize * 4];
            read_bytes += row.len();
            Some(row)
        });
        assert_eq!(read_calls, 1);
        assert_eq!(read_bytes, 65_536);
        register_and_read(9, 0, 0, MAX_SCANLINE_WIDTH + 1, 1);
        sample_frame(5, false, (20000, 1), None, true, |_, _, _| {
            panic!("oversized scanline")
        });
        take_changed_monitors();
        assert_eq!(next_wakeup(), None);
        retain_monitors(&[]);
    }

    #[test]
    fn backend_hooks_precede_windows_and_force_safe_probe_repair() {
        let x = include_str!("../x11/compositor/render.rs");
        let sample = x.find("bar_wallpaper::sample_frame(").unwrap();
        assert!(x.find("self.draw_astronomy_wallpaper(use_scissor").unwrap() < sample);
        assert!(sample < x.find("let restoring_scene;").unwrap());
        assert!(x.contains("first_visible == 0 && !astronomy_occluded && pack_safe"));
        assert!(x.contains("std::mem::take(&mut self.needs_render) || bar_wallpaper_probe"));
        let w = include_str!("../wayland_udev/compositor/render.rs");
        let sample = w.find("bar_wallpaper::sample_frame(").unwrap();
        assert!(
            w.find("self.render_wallpaper(gl, &projection, damage_scissor)")
                .unwrap()
                < sample
        );
        assert!(sample < w.find("// 6. Occlusion culling").unwrap());
        assert!(w.contains("if bar_wallpaper_probe {\n            self.needs_render = true;\n            self.force_full_damage_next = true;"));
        // Wallpaper is explicitly encoded before the separate linear scene pass.
        let wallpaper = include_str!("../wayland_udev/compositor/wallpaper.rs");
        assert!(wallpaper.contains("gl.Uniform1i(self.win_uniforms.scene_linear, 0)"));
        assert!(wallpaper.contains("gl.Uniform1i(self.win_uniforms.color_managed, 0)"));
    }

    #[test]
    fn linear_luminance_is_not_encoded_brightness() {
        assert!((luminance([128, 128, 128, 255]) - 55.04).abs() < 0.1);
        assert_eq!(luminance([0, 0, 0, 255]), 0.0);
    }
    #[test]
    fn global_coordinates_and_y_flip() {
        let p = scanline(
            Rect {
                x: 100,
                y: 50,
                w: 120,
                h: 20,
            },
            (400, 300),
            None,
        )
        .unwrap();
        assert_eq!(p, (100, 239, 120));
        assert_eq!(
            scanline(
                Rect {
                    x: 0,
                    y: 299,
                    w: 1,
                    h: 1
                },
                (400, 300),
                None
            ),
            Some((0, 0, 1))
        );
    }
    #[test]
    fn never_sample_retained_pixels_or_invalid_geometry() {
        let r = Rect {
            x: 100,
            y: 50,
            w: 120,
            h: 20,
        };
        assert!(scanline(r, (400, 300), Some((0, 0, 100, 300))).is_none());
        // Even when all twelve interior CPU sample points would fit, the
        // endpoints between them are not allowed to capture retained windows.
        assert!(scanline(r, (400, 300), Some((101, 239, 118, 1))).is_none());
        assert_eq!(
            scanline(r, (400, 300), Some((100, 239, 120, 1))),
            Some((100, 239, 120))
        );
        assert!(
            scanline(
                Rect {
                    x: 0,
                    y: 0,
                    w: MAX_SCANLINE_WIDTH + 1,
                    h: 1
                },
                (20000, 1),
                None
            )
            .is_none()
        );
        assert!(row_luminance(&[0; 3], 1).is_none());
        assert!(row_luminance(&[], MAX_SCANLINE_WIDTH + 1).is_none());

        assert!(scanline(Rect { x: -1, ..r }, (400, 300), None).is_none());
        assert!(scanline(Rect { w: i32::MAX, ..r }, (400, 300), None).is_none());
    }
    #[test]
    fn hysteresis_holds_near_threshold() {
        let mut e = Entry {
            rect: Rect {
                x: 0,
                y: 0,
                w: 1,
                h: 1,
            },
            pending: true,
            attempt: None,
            sampled: None,
            unavailable: false,
            value: None,
            smoothed: 0.0,
            bright: None,
        };
        e.value = Some(stabilize(&mut e, 44.0));
        for v in [48., 45., 49., 43., 50.] {
            assert!(stabilize(&mut e, v) < 46);
        }
        assert!(stabilize(&mut e, 100.) >= 46);
        for v in [45., 43., 47.] {
            assert!(stabilize(&mut e, v) >= 46);
        }
    }
}
