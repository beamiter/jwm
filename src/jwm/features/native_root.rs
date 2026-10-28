//! Static root wallpaper shared with compositor-off and four-panel sessions.
//!
//! The GPU compositor owns `behavior.wallpaper` while it is running. When it
//! is off the same picture is decoded off-thread, laid out per output with
//! the same Fill/Fit/Stretch/Center rules, and installed as the X11 root
//! pixmap (`_XROOTPMAP_ID`) so gaps and bars see the desktop instead of a
//! solid black root. The four-panel shell also samples this wallpaper-only
//! pixmap while the GPU compositor runs, without capturing application pixels.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::os::unix::fs::MetadataExt as _;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::backend::api::Backend;
use crate::backend::compositor_common::ui_theme;
use crate::backend::compositor_common::wallpaper::{
    MAX_NATIVE_ROOT_BYTES, NativeMonitorBlit, blit_rgba_clipped, compose_native_root,
    compute_wallpaper_rect, parse_wallpaper_mode, resolve_wallpaper_for_tag_at,
};
use crate::config::CONFIG;
use crate::jwm::Jwm;
use crate::jwm::features::connectivity::BackgroundJob;
use crate::jwm::features::wallpaper::expand_home;

fn root_wallpaper_needed(composited: bool, supported: bool, four_panel: bool) -> bool {
    supported && (!composited || four_panel)
}

#[cfg(test)]
mod shell_wallpaper_tests {
    use super::root_wallpaper_needed;

    #[test]
    fn four_panel_glass_gets_a_wallpaper_source_even_with_a_gpu_compositor() {
        assert!(root_wallpaper_needed(true, true, true));
        assert!(!root_wallpaper_needed(true, true, false));
        assert!(root_wallpaper_needed(false, true, false));
        assert!(!root_wallpaper_needed(true, false, true));
        assert!(!root_wallpaper_needed(false, false, true));
    }
}

/// RGBA8 canvas ready to upload as a native root pixmap.
#[derive(Debug)]
pub struct NativeRootPixels {
    pub(crate) key: String,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) rgba: Vec<u8>,
    ack: Option<Arc<AtomicU8>>,
    has_images: bool,
}

struct NativeRootRequest {
    key: String,
    root_w: u32,
    root_h: u32,
    fill: [u8; 3],
    images: Vec<(String, PathBuf)>,
    monitors: Vec<NativeMonitorBlit>,
}

/// No timer is armed for checks: existing maintenance ticks may probe once a
/// second. The receiver survives compositor handoffs; the shared lock confines
/// source signatures and retry state to the background worker.
#[derive(Debug, Default)]
pub struct NativeRootSchedule {
    fingerprint: Option<u64>,
    generation: u64,
    last_started: String,
    last_probe: Option<Instant>,
    has_images: bool,
    launch_retry: Option<(u32, Instant)>,
    worker: Arc<Mutex<NativeRootWorker>>,
    epoch: Arc<AtomicU64>,
}

#[derive(Debug, Default)]
struct NativeRootWorker {
    signature: Option<NativeRootSignature>,
    pending: Option<(NativeRootSignature, Arc<AtomicU8>)>,
    failed: Option<NativeRootSignature>,
    failures: u32,
    retry_at: Option<Instant>,
}

type NativeRootSignature = (String, Vec<String>);

impl NativeRootWorker {
    fn failure(&mut self, signature: NativeRootSignature, now: Instant) {
        self.failures = if self.failed.as_ref() == Some(&signature) {
            self.failures.saturating_add(1)
        } else {
            1
        };
        self.failed = Some(signature);
        let seconds = (1u64 << self.failures.saturating_sub(1).min(5)).min(30);
        self.retry_at = Some(now + Duration::from_secs(seconds));
    }

    fn reconcile_ack(&mut self, now: Instant) {
        if let Some((signature, ack)) = self.pending.take() {
            match ack.load(Ordering::Acquire) {
                1 => {
                    self.signature = Some(signature);
                    self.failed = None;
                    self.failures = 0;
                    self.retry_at = None;
                }
                2 => self.failure(signature, now),
                _ => {} // Superseded/uninstalled candidate is never cached.
            }
        }
    }
}

impl NativeRootSchedule {
    fn request(&mut self, fingerprint: u64, invalidated: bool) -> Option<String> {
        if self.fingerprint != Some(fingerprint) || invalidated {
            self.fingerprint = Some(fingerprint);
            self.launch_retry = None;
            self.generation = self.generation.wrapping_add(1);
            self.epoch.store(self.generation, Ordering::Relaxed);
            Some(format!("{fingerprint:x}:{}", self.generation))
        } else {
            None
        }
    }

    fn due(&self, key: &str, now: Instant, in_flight: bool) -> bool {
        if in_flight {
            return false;
        }
        if self.last_started != key {
            return true;
        }
        if let Some((_, deadline)) = self.launch_retry {
            return now >= deadline;
        }
        self.has_images
            && self
                .last_probe
                .is_none_or(|last| now.duration_since(last) >= Duration::from_secs(1))
    }

    fn launch_failed(&mut self, now: Instant) {
        let failures = self
            .launch_retry
            .map_or(1, |(failures, _)| failures.saturating_add(1));
        let seconds = (1u64 << failures.saturating_sub(1).min(5)).min(30);
        self.launch_retry = Some((failures, now + Duration::from_secs(seconds)));
    }
}

impl Jwm {
    /// Publish the wallpaper for the native root or the shell's glass sampler.
    pub(crate) fn ensure_native_root_wallpaper(&mut self, backend: &mut dyn Backend) {
        if !root_wallpaper_needed(
            backend.has_compositor(),
            backend.native_root_wallpaper_supported(),
            CONFIG.load().status_bar_four_edge(),
        ) {
            return;
        }
        let Some(fingerprint) = self.native_root_fingerprint() else {
            return;
        };
        if let Some(key) = self
            .features
            .native_root_schedule
            .request(fingerprint, self.features.native_root_key.is_empty())
        {
            self.features.native_root_key = key;
        }
        // Keep one receiver until completion. A newer desired identity makes
        // its old pixels unadoptable; the next ensure starts only the newest.
        let now = Instant::now();
        if !self.features.native_root_schedule.due(
            &self.features.native_root_key,
            now,
            self.features.native_root_job.is_some(),
        ) {
            return;
        }
        let Some(mut request) = self.native_root_request() else {
            return;
        };
        request.key.clone_from(&self.features.native_root_key);
        let schedule = &mut self.features.native_root_schedule;
        schedule.has_images = !request.images.is_empty();
        schedule.last_started.clone_from(&request.key);
        schedule.last_probe = Some(now);
        let worker = Arc::clone(&schedule.worker);
        let epoch = Arc::clone(&schedule.epoch);
        let generation = schedule.generation;
        let job = BackgroundJob::spawn(move || {
            catch_native_root_work(|| {
                let mut worker = worker.lock().unwrap_or_else(|poison| poison.into_inner());
                let current = || epoch.load(Ordering::Relaxed) == generation;
                probe_native_root_current(
                    &mut worker,
                    request,
                    wallpaper_file_token,
                    |path| load_native_wallpaper_current(path, &current),
                    &current,
                )
            })
        });
        self.features.native_root_job = job.started().then(|| self.track_background_job(job));
        if self.features.native_root_job.is_none() {
            self.features.native_root_schedule.launch_failed(now);
        } else {
            self.features.native_root_schedule.launch_retry = None;
        }
    }

    /// Cancel the desired generation while retaining its receiver. Fast
    /// compositor handoffs cannot accumulate threads waiting on the worker lock.
    pub(crate) fn cancel_native_root_wallpaper_work(&mut self) {
        self.features.native_root_schedule.generation = self
            .features
            .native_root_schedule
            .generation
            .wrapping_add(1);
        self.features.native_root_schedule.epoch.store(
            self.features.native_root_schedule.generation,
            Ordering::Relaxed,
        );
        self.features.native_root_key.clear();
    }

    /// Adopt a finished native wallpaper decode.
    pub(crate) fn poll_native_root_wallpaper(&mut self, backend: &mut dyn Backend) {
        let Some(result) = self
            .features
            .native_root_job
            .as_ref()
            .and_then(BackgroundJob::take)
        else {
            if self
                .features
                .native_root_job
                .as_ref()
                .is_some_and(|job| !job.started())
            {
                self.features.native_root_job = None;
            }
            return;
        };
        self.features.native_root_job = None;
        if backend.has_compositor() && !CONFIG.load().status_bar_four_edge() {
            return;
        }
        if let Some(pixels) = result {
            if pixels.key != self.features.native_root_key {
                return;
            }
            let has_images = pixels.has_images;
            let installed =
                install_native_root_pixels(backend, &self.features.native_root_key, Some(pixels));
            self.features.native_root_schedule.has_images = has_images || !installed;
        } else {
            // A source/canvas failure has no replacement to install. Keep the
            // current pixmap and let bounded worker probes handle its retry.
            self.features.native_root_schedule.has_images = true;
        }
    }

    /// Hash borrowed configuration paths and output values without allocating
    /// strings or asking the filesystem about any source.
    fn native_root_fingerprint(&self) -> Option<u64> {
        let cfg = CONFIG.load();
        let behavior = cfg.behavior();
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        native_root_canvas_size(self)?.hash(&mut hash);
        ui_theme::palette().native_letterbox_rgb().hash(&mut hash);
        let day = if behavior.wallpaper_rotation == "daily"
            && !behavior.wallpaper_rotation_paths.is_empty()
        {
            self.features
                .wallpaper_rotation
                .observed_day()
                .unwrap_or_else(super::wallpaper_rotation::local_day)
        } else {
            0
        };
        let mut slot = 0;
        for (idx, key) in self.state.monitor_order.iter().enumerate() {
            let Some(monitor) = self.state.monitors.get(*key) else {
                continue;
            };
            let selection = resolve_wallpaper_for_tag_at(
                behavior,
                if behavior.wallpaper_rotation == "daily" {
                    slot
                } else {
                    idx as u32
                },
                monitor.get_active_tags(),
                day,
            );
            slot += 1;
            selection.hash(&mut hash);
            (
                idx,
                monitor.geometry.m_x,
                monitor.geometry.m_y,
                monitor.geometry.m_w,
                monitor.geometry.m_h,
            )
                .hash(&mut hash);
        }
        Some(hash.finish())
    }

    fn native_root_request(&self) -> Option<NativeRootRequest> {
        let cfg = CONFIG.load();
        let behavior = cfg.behavior();
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
        let fill = ui_theme::palette().native_letterbox_rgb();
        let (root_w, root_h) = native_root_canvas_size(self)?;
        let day = if behavior.wallpaper_rotation == "daily"
            && !behavior.wallpaper_rotation_paths.is_empty()
        {
            self.features
                .wallpaper_rotation
                .observed_day()
                .unwrap_or_else(super::wallpaper_rotation::local_day)
        } else {
            0
        };
        let mut images: Vec<(String, PathBuf)> = Vec::new();
        let mut monitors = Vec::new();
        let key = String::new();
        let mut slot = 0_u32;
        for (idx, &mk) in self.state.monitor_order.iter().enumerate() {
            let Some(monitor) = self.state.monitors.get(mk) else {
                continue;
            };
            let (path, mode_str) = resolve_wallpaper_for_tag_at(
                behavior,
                if behavior.wallpaper_rotation == "daily" {
                    slot
                } else {
                    idx as u32
                },
                monitor.get_active_tags(),
                day,
            );
            slot += 1;
            let expanded = expand_home(path, &home);
            if path.trim().is_empty() {
                continue;
            }
            let image_index =
                if let Some(index) = images.iter().position(|(stored, _)| stored == path) {
                    index
                } else {
                    images.push((path.to_string(), expanded));
                    images.len() - 1
                };
            monitors.push(NativeMonitorBlit {
                x: monitor.geometry.m_x,
                y: monitor.geometry.m_y,
                w: monitor.geometry.m_w.max(1) as u32,
                h: monitor.geometry.m_h.max(1) as u32,
                image: image_index,
                mode: parse_wallpaper_mode(mode_str),
            });
        }
        if monitors.is_empty() && images.is_empty() {
            // Still paint the theme letterbox so compositor-off is not the
            // historical solid black root.
            monitors.push(NativeMonitorBlit {
                x: 0,
                y: 0,
                w: root_w,
                h: root_h,
                image: usize::MAX,
                mode: parse_wallpaper_mode("fill"),
            });
        }
        Some(NativeRootRequest {
            key,
            root_w,
            root_h,
            fill,
            images,
            monitors,
        })
    }
}

fn native_root_canvas_size(jwm: &Jwm) -> Option<(u32, u32)> {
    let mut width = jwm.s_w.max(0) as u32;
    let mut height = jwm.s_h.max(0) as u32;
    for &mk in &jwm.state.monitor_order {
        let Some(monitor) = jwm.state.monitors.get(mk) else {
            continue;
        };
        let right = monitor
            .geometry
            .m_x
            .saturating_add(monitor.geometry.m_w)
            .max(0) as u32;
        let bottom = monitor
            .geometry
            .m_y
            .saturating_add(monitor.geometry.m_h)
            .max(0) as u32;
        width = width.max(right);
        height = height.max(bottom);
    }
    if width == 0 || height == 0 {
        return None;
    }
    if width > u16::MAX as u32 || height > u16::MAX as u32 {
        return None;
    }
    Some((width, height))
}

fn wallpaper_file_token(path: &Path) -> String {
    match std::fs::metadata(path) {
        // Seconds and length alone miss same-size edits within one second,
        // atomic replacements and symlink retargeting. Match the source
        // identity used by the composited wallpaper cache.
        Ok(meta) => format!(
            "{}:{}:{}:{}:{}:{}:{}:{}",
            path.display(),
            meta.dev(),
            meta.ino(),
            meta.len(),
            meta.mtime(),
            meta.mtime_nsec(),
            meta.ctime(),
            meta.ctime_nsec(),
        ),
        Err(_) => format!("{}:missing", path.display()),
    }
}

/// BackgroundJob only publishes a returned value. Contain unexpected panics
/// here so every started native job still completes and releases its slot.
fn catch_native_root_work(
    work: impl FnOnce() -> Option<NativeRootPixels>,
) -> Option<NativeRootPixels> {
    match catch_unwind(AssertUnwindSafe(work)) {
        Ok(result) => result,
        Err(_) => {
            log::warn!("native root wallpaper: worker panicked");
            None
        }
    }
}

/// One metadata observation per unique source, on the worker. An unchanged
/// signature never allocates a root canvas and never decodes an image.
#[cfg(test)]
fn probe_native_root(
    worker: &mut NativeRootWorker,
    request: NativeRootRequest,
    token: impl FnMut(&Path) -> String,
    load: impl FnMut(&Path) -> Option<image::RgbaImage>,
) -> Option<NativeRootPixels> {
    probe_native_root_current(worker, request, token, load, &|| true)
}

fn probe_native_root_current(
    worker: &mut NativeRootWorker,
    request: NativeRootRequest,
    token: impl FnMut(&Path) -> String,
    load: impl FnMut(&Path) -> Option<image::RgbaImage>,
    current: &impl Fn() -> bool,
) -> Option<NativeRootPixels> {
    probe_native_root_with_clock(worker, request, token, load, current, &Instant::now)
}

#[cfg(test)]
fn probe_native_root_at(
    worker: &mut NativeRootWorker,
    request: NativeRootRequest,
    token: impl FnMut(&Path) -> String,
    load: impl FnMut(&Path) -> Option<image::RgbaImage>,
    current: &impl Fn() -> bool,
    now: Instant,
) -> Option<NativeRootPixels> {
    probe_native_root_with_clock(worker, request, token, load, current, &|| now)
}

fn probe_native_root_with_clock(
    worker: &mut NativeRootWorker,
    request: NativeRootRequest,
    mut token: impl FnMut(&Path) -> String,
    mut load: impl FnMut(&Path) -> Option<image::RgbaImage>,
    current: &impl Fn() -> bool,
    clock: &impl Fn() -> Instant,
) -> Option<NativeRootPixels> {
    let mut tokens = Vec::with_capacity(request.images.len());
    for (_, path) in &request.images {
        if !current() {
            return None;
        }
        tokens.push(token(path));
        if !current() {
            return None;
        }
    }
    if !current() {
        return None;
    }
    let now = clock();
    worker.reconcile_ack(now);
    let signature = (request.key.clone(), tokens);
    if worker.signature.as_ref() == Some(&signature) {
        worker.failed = None;
        worker.failures = 0;
        worker.retry_at = None;
        return None;
    }
    if worker.failed.as_ref() == Some(&signature) {
        if worker.retry_at.is_some_and(|deadline| now < deadline) {
            return None;
        }
    } else {
        worker.failed = None;
        worker.failures = 0;
        worker.retry_at = None;
    }
    let failed = std::cell::Cell::new(false);
    let decoded = catch_unwind(AssertUnwindSafe(|| {
        decode_native_root_with_current(
            request,
            |path| {
                let image = load(path);
                failed.set(failed.get() || image.is_none());
                image
            },
            current,
            |rgba, w, h, image, slot| {
                failed.set(
                    !blit_native_wallpaper_current(rgba, w, h, image, slot, current)
                        || failed.get(),
                );
            },
        )
    }));
    let mut result = match decoded {
        Ok(result) => result,
        Err(_) => {
            log::warn!("native root wallpaper: decode or resize panicked");
            failed.set(true);
            None
        }
    };
    if !current() {
        return None;
    }
    if failed.get() || result.is_none() {
        // Slow I/O or codecs must not consume the retry delay themselves.
        worker.failure(signature, clock());
        return None;
    }
    let ack = Arc::new(AtomicU8::new(0));
    result.as_mut().unwrap().ack = Some(Arc::clone(&ack));
    worker.pending = Some((signature, ack));
    result
}

fn load_native_wallpaper_current(
    path: &Path,
    current: &impl Fn() -> bool,
) -> Option<image::RgbaImage> {
    if !current() {
        return None;
    }
    let image =
        crate::backend::compositor_common::image_source::open_regular_image_detected(path).ok()?;
    // A codec may have been running while the desired generation changed.
    // Avoid a full-resolution RGBA conversion for the obsolete source.
    current().then(|| image.into_rgba8())
}

#[cfg(test)]
fn load_native_wallpaper(path: &Path) -> Option<image::RgbaImage> {
    load_native_wallpaper_current(path, &|| true)
}

fn install_native_root_pixels(
    backend: &mut dyn Backend,
    expected_key: &str,
    pixels: Option<NativeRootPixels>,
) -> bool {
    let Some(pixels) = pixels else {
        log::warn!("native root wallpaper: decode produced no image");
        return false;
    };
    if pixels.key != expected_key {
        return false;
    }
    let installed = match backend.install_root_wallpaper(pixels.width, pixels.height, &pixels.rgba)
    {
        Ok(()) => true,
        Err(error) => {
            log::warn!("native root wallpaper: {error}");
            false
        }
    };
    if let Some(ack) = pixels.ack {
        ack.store(if installed { 1 } else { 2 }, Ordering::Release);
    }
    installed
}

#[cfg(test)]
fn decode_native_root(request: NativeRootRequest) -> Option<NativeRootPixels> {
    decode_native_root_with(request, load_native_wallpaper)
}

#[cfg(test)]
fn decode_native_root_with(
    request: NativeRootRequest,
    load: impl FnMut(&Path) -> Option<image::RgbaImage>,
) -> Option<NativeRootPixels> {
    decode_native_root_with_current(request, load, &|| true, blit_native_wallpaper)
}

fn decode_native_root_with_current(
    request: NativeRootRequest,
    mut load: impl FnMut(&Path) -> Option<image::RgbaImage>,
    current: &impl Fn() -> bool,
    mut blit: impl FnMut(&mut [u8], u32, u32, &image::RgbaImage, &NativeMonitorBlit),
) -> Option<NativeRootPixels> {
    if !current() {
        return None;
    }
    // Reject an invalid canvas before touching any source files. Decode on
    // first use, then release each source after its last output: a desktop
    // with different pictures should not retain every full-resolution image
    // while allocating and resampling its root canvas.
    let mut rgba = compose_native_root(request.root_w, request.root_h, request.fill, &[], &[])?;
    let mut remaining = HashMap::<usize, usize>::new();
    for slot in &request.monitors {
        *remaining.entry(slot.image).or_default() += 1;
    }
    let mut decoded = HashMap::new();
    for slot in &request.monitors {
        if !current() {
            return None;
        }
        let Some((_, path)) = request.images.get(slot.image) else {
            continue;
        };
        let image = decoded.entry(slot.image).or_insert_with(|| {
            let image = load(path);
            if image.is_none() {
                log::warn!("native root wallpaper: failed to decode {}", path.display());
            }
            image
        });
        if !current() {
            return None;
        }
        if let Some(image) = image {
            blit(&mut rgba, request.root_w, request.root_h, image, slot);
        }
        let uses = remaining.get_mut(&slot.image).expect("counted output");
        *uses -= 1;
        if *uses == 0 {
            decoded.remove(&slot.image);
        }
    }
    if !current() {
        return None;
    }
    Some(NativeRootPixels {
        key: request.key,
        width: request.root_w,
        height: request.root_h,
        rgba,
        ack: None,
        has_images: !request.images.is_empty(),
    })
}

#[cfg(test)]
fn blit_native_wallpaper(
    rgba: &mut [u8],
    root_w: u32,
    root_h: u32,
    image: &image::RgbaImage,
    slot: &NativeMonitorBlit,
) {
    blit_native_wallpaper_current(rgba, root_w, root_h, image, slot, &|| true);
}

fn blit_native_wallpaper_current(
    rgba: &mut [u8],
    root_w: u32,
    root_h: u32,
    image: &image::RgbaImage,
    slot: &NativeMonitorBlit,
    current: &impl Fn() -> bool,
) -> bool {
    if !current() {
        return false;
    }
    let (dx, dy, dw, dh) = compute_wallpaper_rect(
        slot.mode,
        (slot.x as f32, slot.y as f32, slot.w as f32, slot.h as f32),
        image.width(),
        image.height(),
    );
    if !dw.is_finite() || !dh.is_finite() || dw <= 0.0 || dh <= 0.0 {
        return false;
    }
    let tw = dw.round().max(1.0) as u32;
    let th = dh.round().max(1.0) as u32;
    if u64::from(tw)
        .saturating_mul(u64::from(th))
        .saturating_mul(4)
        > MAX_NATIVE_ROOT_BYTES
    {
        return false;
    }
    let placed;
    let image = if tw == image.width() && th == image.height() {
        image
    } else {
        if !current() {
            return false;
        }
        placed = image::imageops::resize(image, tw, th, image::imageops::FilterType::Triangle);
        &placed
    };
    if !current() {
        return false;
    }
    blit_rgba_clipped(
        rgba,
        root_w,
        root_h,
        image.as_raw(),
        image.width(),
        image.height(),
        dx.round() as i32,
        dy.round() as i32,
        (slot.x, slot.y, slot.w, slot.h),
    );
    true
}

#[cfg(test)]
mod tests {
    use super::NativeRootRequest;
    use super::decode_native_root;
    use crate::backend::compositor_common::wallpaper::{NativeMonitorBlit, WallpaperMode};
    use std::path::{Path, PathBuf};

    fn temp_dir() -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "jwm-native-root-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir(&dir).unwrap();
        dir
    }

    fn slot(x: i32, image: usize) -> NativeMonitorBlit {
        NativeMonitorBlit {
            x,
            y: 0,
            w: 2,
            h: 1,
            image,
            mode: WallpaperMode::Stretch,
        }
    }

    fn retry_request(key: &str) -> NativeRootRequest {
        NativeRootRequest {
            key: key.into(),
            root_w: 4,
            root_h: 1,
            fill: [1, 2, 3],
            images: vec![("a".into(), "a.png".into()), ("b".into(), "b.png".into())],
            monitors: vec![slot(0, 0), slot(2, 1)],
        }
    }

    #[test]
    fn panicking_native_decode_keeps_the_installed_canvas_and_retries_after_completion() {
        use crate::jwm::features::monitor_lock::test_support::LockSpyBackend;
        use std::cell::Cell;
        let mut backend = LockSpyBackend::new();
        let mut worker = super::NativeRootWorker::default();
        let start = std::time::Instant::now();
        let now = Cell::new(start);
        let installed = super::probe_native_root_at(
            &mut worker,
            retry_request("current"),
            |_| "v1".into(),
            |_| {
                Some(image::RgbaImage::from_pixel(
                    2,
                    1,
                    image::Rgba([4, 5, 6, 255]),
                ))
            },
            &|| true,
            start,
        )
        .unwrap();
        assert!(super::install_native_root_pixels(
            &mut backend,
            "current",
            Some(installed)
        ));
        let old_canvas = backend.native_root_uploads.clone();
        assert!(
            super::probe_native_root_with_clock(
                &mut worker,
                retry_request("current"),
                |_| "v2".into(),
                |_| {
                    now.set(start + std::time::Duration::from_secs(5));
                    panic!("injected codec/resize panic");
                },
                &|| true,
                &|| now.get()
            )
            .is_none()
        );
        assert_eq!(
            worker.retry_at,
            Some(start + std::time::Duration::from_secs(6))
        );
        assert_eq!(worker.signature.as_ref().unwrap().1, ["v1", "v1"]);
        assert_eq!(
            backend.native_root_uploads, old_canvas,
            "panic never installs partial fill"
        );
        assert!(
            super::probe_native_root_at(
                &mut worker,
                retry_request("current"),
                |_| "v2".into(),
                |_| panic!("panic backoff must prevent immediate re-entry"),
                &|| true,
                start + std::time::Duration::from_millis(5999)
            )
            .is_none()
        );
        let recovered = super::probe_native_root_at(
            &mut worker,
            retry_request("current"),
            |_| "v2".into(),
            |_| {
                Some(image::RgbaImage::from_pixel(
                    2,
                    1,
                    image::Rgba([40, 50, 60, 255]),
                ))
            },
            &|| true,
            start + std::time::Duration::from_secs(6),
        )
        .unwrap();
        assert!(super::install_native_root_pixels(
            &mut backend,
            "current",
            Some(recovered)
        ));
        assert_eq!(backend.native_root_uploads.len(), 2);
        assert_ne!(
            backend.native_root_uploads[0],
            backend.native_root_uploads[1]
        );
    }

    #[test]
    fn unexpected_native_worker_panic_publishes_a_completed_none() {
        let job = crate::jwm::features::connectivity::BackgroundJob::spawn(|| {
            super::catch_native_root_work(|| panic!("injected metadata panic"))
        });
        assert!(job.started());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Some(result) = job.take() {
                assert!(
                    result.is_none(),
                    "panic completes the receiver rather than parking it forever"
                );
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "panicking worker must publish completion"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    #[test]
    fn native_retry_delay_begins_after_a_slow_failed_decode_finishes() {
        use std::cell::Cell;
        let start = std::time::Instant::now();
        let now = Cell::new(start);
        let mut worker = super::NativeRootWorker::default();
        let request = || {
            let mut request = retry_request("current");
            request.images.truncate(1);
            request.monitors.retain(|slot| slot.image == 0);
            request
        };
        let decodes = Cell::new(0);
        assert!(
            super::probe_native_root_with_clock(
                &mut worker,
                request(),
                |_| "same-version".into(),
                |_| {
                    decodes.set(decodes.get() + 1);
                    now.set(start + std::time::Duration::from_secs(5));
                    None
                },
                &|| true,
                &|| now.get(),
            )
            .is_none()
        );
        assert_eq!(
            worker.retry_at,
            Some(start + std::time::Duration::from_secs(6))
        );
        for millis in [5000, 5500, 5999] {
            now.set(start + std::time::Duration::from_millis(millis));
            assert!(
                super::probe_native_root_with_clock(
                    &mut worker,
                    request(),
                    |_| "same-version".into(),
                    |_| panic!("the slow codec must still receive a full retry delay"),
                    &|| true,
                    &|| now.get(),
                )
                .is_none()
            );
        }
        assert_eq!(decodes.get(), 1);
        now.set(start + std::time::Duration::from_secs(6));
        assert!(
            super::probe_native_root_with_clock(
                &mut worker,
                request(),
                |_| "same-version".into(),
                |_| {
                    decodes.set(decodes.get() + 1);
                    None
                },
                &|| true,
                &|| now.get(),
            )
            .is_none()
        );
        assert_eq!(decodes.get(), 2);
        assert_eq!(
            worker.retry_at,
            Some(start + std::time::Duration::from_secs(8))
        );
    }

    #[test]
    fn native_upload_failure_is_not_cached_and_recovers_without_file_changes() {
        use crate::jwm::features::monitor_lock::test_support::LockSpyBackend;
        let mut backend = LockSpyBackend::new();
        backend.native_root_install_failures = 1;
        backend.native_root_uploads.push(vec![99; 16]);
        let mut worker = super::NativeRootWorker::default();
        let now = std::time::Instant::now();
        let pixels = super::probe_native_root_at(
            &mut worker,
            retry_request("current"),
            |_| "unchanged".into(),
            |_| {
                Some(image::RgbaImage::from_pixel(
                    2,
                    1,
                    image::Rgba([5, 6, 7, 255]),
                ))
            },
            &|| true,
            now,
        )
        .unwrap();
        assert!(
            worker.signature.is_none(),
            "decode is not an installation ack"
        );
        assert!(!super::install_native_root_pixels(
            &mut backend,
            "current",
            Some(pixels)
        ));
        assert_eq!(
            backend.native_root_uploads,
            [vec![99; 16]],
            "failed upload keeps the installed root"
        );
        assert!(
            super::probe_native_root_at(
                &mut worker,
                retry_request("current"),
                |_| "unchanged".into(),
                |_| panic!("failed upload enters backoff"),
                &|| true,
                now
            )
            .is_none()
        );
        assert!(worker.signature.is_none());
        let recovered = super::probe_native_root_at(
            &mut worker,
            retry_request("current"),
            |_| "unchanged".into(),
            |_| {
                Some(image::RgbaImage::from_pixel(
                    2,
                    1,
                    image::Rgba([5, 6, 7, 255]),
                ))
            },
            &|| true,
            now + std::time::Duration::from_secs(1),
        )
        .unwrap();
        assert!(super::install_native_root_pixels(
            &mut backend,
            "current",
            Some(recovered)
        ));
        assert!(
            super::probe_native_root_at(
                &mut worker,
                retry_request("current"),
                |_| "unchanged".into(),
                |_| panic!("installed unchanged images must not decode"),
                &|| true,
                now + std::time::Duration::from_secs(2)
            )
            .is_none()
        );
        assert!(worker.signature.is_some());
        assert!(worker.retry_at.is_none());
        assert_eq!(backend.native_root_install_calls, 2);
        assert_eq!(backend.native_root_uploads.len(), 2);
    }

    #[test]
    fn one_failed_output_keeps_the_old_canvas_and_same_version_retries_after_backoff() {
        use crate::jwm::features::monitor_lock::test_support::LockSpyBackend;
        let mut backend = LockSpyBackend::new();
        let mut worker = super::NativeRootWorker::default();
        let now = std::time::Instant::now();
        let first = super::probe_native_root_at(
            &mut worker,
            retry_request("current"),
            |_| "v1".into(),
            |_| {
                Some(image::RgbaImage::from_pixel(
                    2,
                    1,
                    image::Rgba([20, 30, 40, 255]),
                ))
            },
            &|| true,
            now,
        )
        .unwrap();
        assert!(super::install_native_root_pixels(
            &mut backend,
            "current",
            Some(first)
        ));
        let old = backend.native_root_uploads.clone();
        assert!(
            super::probe_native_root_at(
                &mut worker,
                retry_request("current"),
                |_| "v2".into(),
                |path| {
                    if path == Path::new("b.png") {
                        None
                    } else {
                        Some(image::RgbaImage::new(2, 1))
                    }
                },
                &|| true,
                now
            )
            .is_none()
        );
        assert_eq!(
            backend.native_root_uploads, old,
            "one bad screen never installs partial fill over a working desktop"
        );
        assert!(
            super::probe_native_root_at(
                &mut worker,
                retry_request("current"),
                |_| "v2".into(),
                |_| panic!("retry before deadline must not decode"),
                &|| true,
                now + std::time::Duration::from_millis(999)
            )
            .is_none()
        );
        let recovered = super::probe_native_root_at(
            &mut worker,
            retry_request("current"),
            |_| "v2".into(),
            |_| {
                Some(image::RgbaImage::from_pixel(
                    2,
                    1,
                    image::Rgba([50, 60, 70, 255]),
                ))
            },
            &|| true,
            now + std::time::Duration::from_secs(1),
        )
        .unwrap();
        assert!(super::install_native_root_pixels(
            &mut backend,
            "current",
            Some(recovered)
        ));
        assert_eq!(backend.native_root_uploads.len(), 2);
        assert_ne!(
            backend.native_root_uploads[0],
            backend.native_root_uploads[1]
        );
    }

    #[test]
    fn native_retry_backoff_caps_and_source_changes_or_f5_reset_it() {
        let mut worker = super::NativeRootWorker::default();
        let now = std::time::Instant::now();
        for seconds in [1, 2, 4, 8, 16, 30, 30] {
            worker.failure(("current".into(), vec!["v1".into(), "v1".into()]), now);
            assert_eq!(
                worker.retry_at,
                Some(now + std::time::Duration::from_secs(seconds))
            );
        }
        let changed = super::probe_native_root_at(
            &mut worker,
            retry_request("current"),
            |_| "v2".into(),
            |_| Some(image::RgbaImage::new(2, 1)),
            &|| true,
            now,
        )
        .unwrap();
        assert_eq!(
            worker.failures, 0,
            "file version changes bypass the old delay"
        );
        let old_ack = std::sync::Arc::clone(changed.ack.as_ref().unwrap());
        drop(changed);
        worker.failure(("current".into(), vec!["v2".into(), "v2".into()]), now);
        let forced = super::probe_native_root_at(
            &mut worker,
            retry_request("f5-generation"),
            |_| "v2".into(),
            |_| Some(image::RgbaImage::new(2, 1)),
            &|| true,
            now,
        )
        .unwrap();
        assert_eq!(
            worker.failures, 0,
            "F5/input generation bypasses the old delay"
        );
        // A late ack from the superseded candidate cannot acknowledge the new
        // generation: only the worker's newest pending identity is consumed.
        old_ack.store(1, super::Ordering::Release);
        forced
            .ack
            .as_ref()
            .unwrap()
            .store(1, super::Ordering::Release);
        worker.reconcile_ack(now);
        assert_eq!(worker.signature.as_ref().unwrap().0, "f5-generation");
        let mut schedule = super::NativeRootSchedule::default();
        let key = schedule.request(1, true).unwrap();
        schedule.last_started = key.clone();
        for seconds in [1, 2, 4, 8, 16, 30] {
            schedule.launch_failed(now);
            assert!(!schedule.due(&key, now, false));
            assert!(schedule.due(&key, now + std::time::Duration::from_secs(seconds), false));
        }
        let force = schedule.request(1, true).unwrap();
        assert!(
            schedule.due(&force, now, false),
            "F5 also clears thread-start backoff"
        );
    }

    #[test]
    fn obsolete_native_probes_skip_decode_and_never_commit_a_signature() {
        use std::cell::Cell;
        let request = || NativeRootRequest {
            key: "new".into(),
            root_w: 4,
            root_h: 1,
            fill: [0, 0, 0],
            images: vec![("a".into(), "a.png".into())],
            monitors: vec![slot(0, 0), slot(2, 0)],
        };
        let current = Cell::new(true);
        let mut worker = super::NativeRootWorker::default();
        worker.signature = Some(("previous".into(), vec!["old".into()]));
        let previous = worker.signature.clone();
        assert!(
            super::probe_native_root_current(
                &mut worker,
                request(),
                |_| {
                    current.set(false);
                    "stat".into()
                },
                |_| panic!("cancelled metadata must not decode"),
                &|| current.get()
            )
            .is_none()
        );
        assert_eq!(worker.signature, previous);
        current.set(true);
        assert!(
            super::probe_native_root_current(
                &mut worker,
                request(),
                |_| "stat".into(),
                |_| {
                    current.set(false);
                    Some(image::RgbaImage::new(2, 1))
                },
                &|| current.get()
            )
            .is_none()
        );
        assert_eq!(
            worker.signature, previous,
            "cancellation after codec cannot publish a signature"
        );
        current.set(true);
        let mut placements = 0;
        assert!(
            super::decode_native_root_with_current(
                request(),
                |_| { Some(image::RgbaImage::new(2, 1)) },
                &|| current.get(),
                |_, _, _, _, _| {
                    placements += 1;
                    current.set(false);
                }
            )
            .is_none()
        );
        assert_eq!(
            placements, 1,
            "cancelling a placement skips all later resizes"
        );
        current.set(true);
        assert!(
            super::decode_native_root_with_current(
                request(),
                |_| {
                    current.set(false);
                    Some(image::RgbaImage::new(2, 1))
                },
                &|| current.get(),
                |_, _, _, _, _| panic!("cancelled decode must not resize")
            )
            .is_none()
        );
    }

    #[test]
    fn fast_compositor_handoffs_retain_one_receiver_and_only_the_latest_request() {
        use crate::jwm::features::connectivity::BackgroundJob;
        use crate::jwm::features::monitor_lock::test_support::LockSpyBackend;
        let mut backend = LockSpyBackend::new();
        let mut jwm = crate::jwm::Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let (job, finish) = BackgroundJob::controlled_for_test();
        jwm.features.native_root_job = Some(job);
        let now = std::time::Instant::now();
        for fingerprint in 0..100 {
            jwm.cancel_native_root_wallpaper_work();
            assert!(jwm.features.native_root_key.is_empty());
            let latest = jwm
                .features
                .native_root_schedule
                .request(fingerprint, true)
                .unwrap();
            jwm.features.native_root_key = latest;
            assert!(
                !jwm.features.native_root_schedule.due(
                    &jwm.features.native_root_key,
                    now,
                    jwm.features.native_root_job.is_some()
                ),
                "off transition waits for the one existing receiver"
            );
        }
        finish(None);
        assert_eq!(
            jwm.features
                .native_root_job
                .as_ref()
                .unwrap()
                .take()
                .map(|value| value.is_none()),
            Some(true),
            "all handoffs retained the original controlled receiver"
        );
        // The controlled completion was consumed above; remove only after it
        // completed, just as poll_native_root_wallpaper does in production.
        jwm.features.native_root_job = None;
        assert!(
            jwm.features
                .native_root_schedule
                .due(&jwm.features.native_root_key, now, false)
        );
    }

    #[test]
    fn native_probe_work_is_bounded_and_changed_files_or_explicit_refresh_decode() {
        let mut worker = super::NativeRootWorker::default();
        let request = |key: &str| NativeRootRequest {
            key: key.into(),
            root_w: 4,
            root_h: 1,
            fill: [0, 0, 0],
            images: vec![("same".into(), "same.png".into())],
            monitors: vec![slot(0, 0), slot(2, 0)],
        };
        let mut stats = 0;
        let mut decodes = 0;
        for (key, file_version, expected_pixels) in [
            ("layout:1", "v1", true),
            ("layout:1", "v1", false),
            ("layout:1", "v2", true),
            ("layout:2", "v2", true),
        ] {
            let result = super::probe_native_root(
                &mut worker,
                request(key),
                |_| {
                    stats += 1;
                    file_version.into()
                },
                |_| {
                    decodes += 1;
                    Some(image::RgbaImage::from_pixel(
                        2,
                        1,
                        image::Rgba([5, 0, 0, 255]),
                    ))
                },
            );
            assert_eq!(result.is_some(), expected_pixels);
            if let Some(pixels) = result {
                pixels.ack.unwrap().store(1, super::Ordering::Release);
            }
        }
        assert_eq!(stats, 4, "one stat per unique source, not per output");
        assert_eq!(
            decodes, 3,
            "unchanged probes do no decode or canvas composition"
        );
    }

    #[test]
    fn native_schedule_skips_frame_rate_probes_and_coalesces_to_the_latest_identity() {
        let mut schedule = super::NativeRootSchedule::default();
        let now = std::time::Instant::now();
        let first = schedule.request(1, true).unwrap();
        schedule.last_started = first.clone();
        schedule.last_probe = Some(now);
        schedule.has_images = true;
        let mut probes = 0;
        for frame in 0..1000 {
            assert!(schedule.request(1, false).is_none());
            probes += usize::from(schedule.due(
                &first,
                now + std::time::Duration::from_micros(frame * 999),
                false,
            ));
        }
        assert_eq!(
            probes, 0,
            "1000 unchanged ticks schedule zero filesystem workers"
        );
        assert!(schedule.due(&first, now + std::time::Duration::from_secs(1), false));
        let superseded = schedule.request(2, false).unwrap();
        let newest = schedule.request(3, false).unwrap();
        assert_ne!(newest, superseded);
        assert!(
            schedule.due(&newest, now, false),
            "changed output bypasses the probe interval"
        );
        for _ in 0..1000 {
            assert!(
                !schedule.due(&newest, now, true),
                "superseding requests keep at most one active job"
            );
        }
        let explicit = schedule.request(3, true).unwrap();
        assert_ne!(
            explicit, newest,
            "F5 forces a fresh signature even if stat is unchanged"
        );
        schedule.last_started = explicit.clone();
        schedule.has_images = false;
        assert!(
            !schedule.due(&explicit, now + std::time::Duration::from_secs(99), false),
            "a solid root has no files to poll"
        );
    }

    #[test]
    fn native_decode_reuses_sources_and_preserves_overlapping_output_order() {
        let mut loaded = Vec::new();
        let pixels = super::decode_native_root_with(
            NativeRootRequest {
                key: "overlap".into(),
                root_w: 4,
                root_h: 1,
                fill: [0, 0, 0],
                images: vec![
                    ("a".into(), "a.png".into()),
                    ("b".into(), "b.png".into()),
                    ("unused".into(), "unused.png".into()),
                ],
                monitors: vec![slot(0, 0), slot(1, 1), slot(2, 0)],
            },
            |path| {
                loaded.push(path.to_owned());
                let color = if path == Path::new("a.png") { 10 } else { 20 };
                Some(image::RgbaImage::from_pixel(
                    2,
                    1,
                    image::Rgba([color, 0, 0, 255]),
                ))
            },
        )
        .unwrap();
        assert_eq!(loaded, [PathBuf::from("a.png"), PathBuf::from("b.png")]);
        let red: Vec<_> = pixels.rgba.chunks_exact(4).map(|pixel| pixel[0]).collect();
        assert_eq!(red, [10, 20, 10, 10]);
    }

    #[test]
    fn native_decode_skips_sources_for_an_invalid_canvas_and_remembers_failures() {
        let request = |root_w| NativeRootRequest {
            key: "invalid".into(),
            root_w,
            root_h: 1,
            fill: [1, 2, 3],
            images: vec![("missing".into(), "missing.png".into())],
            monitors: vec![slot(0, 0), slot(0, 0)],
        };
        assert!(super::decode_native_root_with(request(0), |_| panic!("invalid canvas")).is_none());
        assert!(
            super::decode_native_root_with(request(u32::MAX), |_| panic!("oversized canvas"))
                .is_none()
        );
        let mut attempts = 0;
        let pixels = super::decode_native_root_with(request(1), |_| {
            attempts += 1;
            None
        })
        .unwrap();
        assert_eq!(attempts, 1);
        assert_eq!(pixels.rgba, [1, 2, 3, 255]);
    }

    #[test]
    fn native_wallpaper_applies_embedded_orientation_before_layout() {
        use image::ImageEncoder as _;
        let dir = temp_dir();
        let path = dir.join("oriented.png");
        // TIFF Orientation=6: a 2x1 source rotates clockwise to 1x2.
        let exif = vec![
            b'I', b'I', 42, 0, 8, 0, 0, 0, 1, 0, 0x12, 0x01, 3, 0, 1, 0, 0, 0, 6, 0, 0, 0, 0, 0, 0,
            0,
        ];
        let mut encoded = Vec::new();
        let mut encoder = image::codecs::png::PngEncoder::new(&mut encoded);
        encoder.set_exif_metadata(exif).unwrap();
        encoder
            .write_image(
                &[255, 0, 0, 0, 0, 255],
                2,
                1,
                image::ExtendedColorType::Rgb8,
            )
            .unwrap();
        std::fs::write(&path, encoded).unwrap();
        let pixels = decode_native_root(NativeRootRequest {
            key: "orientation".into(),
            root_w: 1,
            root_h: 2,
            fill: [0, 0, 0],
            images: vec![("oriented".into(), path)],
            monitors: vec![NativeMonitorBlit {
                x: 0,
                y: 0,
                w: 1,
                h: 2,
                image: 0,
                mode: WallpaperMode::Center,
            }],
        })
        .unwrap();
        assert_eq!(pixels.rgba, [255, 0, 0, 255, 0, 0, 255, 255]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn native_wallpaper_rejects_oversized_sources() {
        let dir = temp_dir();
        let path = dir.join("oversized.png");
        std::fs::File::create(&path)
            .unwrap()
            .set_len(64 * 1024 * 1024 + 1)
            .unwrap();
        assert!(super::load_native_wallpaper(&path).is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn native_source_token_detects_subsecond_edits_and_atomic_replacement() {
        let dir = temp_dir();
        let path = dir.join("wallpaper.png");
        std::fs::write(&path, b"aaaa").unwrap();
        let modified = std::time::UNIX_EPOCH + std::time::Duration::new(1_700_000_000, 100);
        let set_time = |path: &Path, time| {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(time))
                .unwrap();
        };
        set_time(&path, modified);
        let original = super::wallpaper_file_token(&path);
        std::fs::write(&path, b"bbbb").unwrap();
        set_time(&path, modified + std::time::Duration::from_nanos(100));
        let edited = super::wallpaper_file_token(&path);
        assert_ne!(original, edited);
        let replacement = dir.join("replacement.png");
        std::fs::write(&replacement, b"cccc").unwrap();
        set_time(
            &replacement,
            modified + std::time::Duration::from_nanos(100),
        );
        std::fs::rename(&replacement, &path).unwrap();
        assert_ne!(edited, super::wallpaper_file_token(&path));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn native_transparent_wallpaper_blends_over_the_theme_letterbox() {
        let dir = temp_dir();
        let path = dir.join("transparent.png");
        image::RgbaImage::from_pixel(1, 1, image::Rgba([200, 100, 50, 0]))
            .save(&path)
            .unwrap();
        let pixels = decode_native_root(NativeRootRequest {
            key: "transparent".into(),
            root_w: 1,
            root_h: 1,
            fill: [9, 8, 7],
            images: vec![("transparent".into(), path)],
            monitors: vec![slot(0, 0)],
        })
        .unwrap();
        assert_eq!(pixels.rgba, [9, 8, 7, 255]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_missing_wallpaper_file_still_fills_the_letterbox() {
        let pixels = decode_native_root(NativeRootRequest {
            key: "test".into(),
            root_w: 2,
            root_h: 1,
            fill: [1, 2, 3],
            images: vec![(
                "missing".into(),
                std::path::PathBuf::from("/no/such/wallpaper.png"),
            )],
            monitors: vec![NativeMonitorBlit {
                x: 0,
                y: 0,
                w: 2,
                h: 1,
                image: 0,
                mode: WallpaperMode::Fill,
            }],
        })
        .expect("letterbox canvas");
        assert_eq!(pixels.key, "test");
        assert_eq!(pixels.rgba, vec![1, 2, 3, 255, 1, 2, 3, 255]);
    }

    #[test]
    fn an_empty_image_list_still_fills_the_letterbox() {
        let pixels = decode_native_root(NativeRootRequest {
            key: "fill".into(),
            root_w: 1,
            root_h: 1,
            fill: [9, 8, 7],
            images: vec![],
            monitors: vec![NativeMonitorBlit {
                x: 0,
                y: 0,
                w: 1,
                h: 1,
                image: usize::MAX,
                mode: WallpaperMode::Fill,
            }],
        })
        .expect("letterbox canvas");
        assert_eq!(pixels.key, "fill");
        assert_eq!(pixels.rgba, vec![9, 8, 7, 255]);
    }

    #[test]
    fn wallpaper_file_token_marks_a_missing_path() {
        assert!(
            super::wallpaper_file_token(&std::path::PathBuf::from("/no/such/wallpaper.png"))
                .ends_with(":missing")
        );
    }
}
