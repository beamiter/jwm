//! Shared bounded, lazy wallpaper decode slots. Dropping a queued slot never spawns work.
use super::wallpaper::{WallpaperImageData, WallpaperMode};
use std::collections::VecDeque;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock, Weak};
/// Counting gate bounding how many wallpaper images decode concurrently.
/// Each decode does `image::open` + a Lanczos3 downscale (heavy CPU, transient
/// full-image allocation); rapid wallpaper changes or per-monitor setup would
/// otherwise run unbounded decodes at once.
struct DecodeGate {
    /// The number of currently-available decode permits.
    available: Mutex<usize>,
}

impl DecodeGate {
    const fn new(permits: usize) -> Self {
        Self {
            available: Mutex::new(permits),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, usize> {
        self.available.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn try_acquire(&self) -> Option<DecodePermit<'_>> {
        let mut available = self.lock();
        if *available == 0 {
            return None;
        }
        *available -= 1;
        Some(DecodePermit { gate: self })
    }
}

/// The process-wide decode gate shared by wallpaper loads and side previews.
fn decode_gate() -> &'static DecodeGate {
    static GATE: OnceLock<DecodeGate> = OnceLock::new();
    GATE.get_or_init(|| {
        let max = std::thread::available_parallelism()
            .map(|n| n.get().min(4))
            .unwrap_or(2);
        DecodeGate::new(max)
    })
}

/// RAII permit for a [`DecodeGate`]; returns the permit on drop (covering
/// early returns and panics).
struct DecodePermit<'a> {
    gate: &'a DecodeGate,
}

impl Drop for DecodePermit<'_> {
    fn drop(&mut self) {
        let mut available = self.gate.lock();
        *available += 1;
    }
}

type WallpaperDecode = Box<dyn FnOnce(&AtomicBool) -> Option<WallpaperImageData> + Send>;
type DecodeWorker = Box<dyn FnOnce() + Send>;

enum WallpaperLoadState {
    Queued(WallpaperDecode),
    Running(mpsc::Receiver<WallpaperImageData>),
    Failed,
}

/// A per-request CPU decode queue slot. Polling starts a worker only after a
/// permit is available, so replaced requests never leave threads waiting on
/// the gate. Each monitor retains its own slot and result; no global latest-
/// request policy is applied to real wallpapers.
pub(crate) struct DeferredWallpaperLoad {
    state: Mutex<WallpaperLoadState>,
    cancelled: Arc<AtomicBool>,
}

impl DeferredWallpaperLoad {
    pub(crate) fn new(
        decode: impl FnOnce(&AtomicBool) -> Option<WallpaperImageData> + Send + 'static,
    ) -> Self {
        Self {
            state: Mutex::new(WallpaperLoadState::Queued(Box::new(decode))),
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn try_recv(&self) -> Result<WallpaperImageData, mpsc::TryRecvError> {
        self.try_recv_with(decode_gate(), |worker| {
            std::thread::Builder::new()
                .name("jwm-wallpaper".into())
                .spawn(worker)
                .map(|_| ())
        })
    }

    fn try_recv_with(
        &self,
        gate: &'static DecodeGate,
        spawn: impl FnOnce(DecodeWorker) -> std::io::Result<()>,
    ) -> Result<WallpaperImageData, mpsc::TryRecvError> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(*state, WallpaperLoadState::Queued(_)) {
            let Some(permit) = gate.try_acquire() else {
                return Err(mpsc::TryRecvError::Empty);
            };
            let WallpaperLoadState::Queued(decode) =
                std::mem::replace(&mut *state, WallpaperLoadState::Failed)
            else {
                unreachable!();
            };
            let (tx, rx) = mpsc::channel();
            let cancelled = Arc::clone(&self.cancelled);
            let worker = Box::new(move || {
                let _permit = permit;
                if !cancelled.load(Ordering::Acquire)
                    && let Some(image) = decode(&cancelled)
                    && !cancelled.load(Ordering::Acquire)
                {
                    let _ = tx.send(image);
                }
            });
            match spawn(worker) {
                Ok(()) => *state = WallpaperLoadState::Running(rx),
                Err(error) => log::warn!("compositor: could not start wallpaper decoder: {error}"),
            }
        }
        match &*state {
            WallpaperLoadState::Running(rx) => rx.try_recv(),
            WallpaperLoadState::Failed => Err(mpsc::TryRecvError::Disconnected),
            WallpaperLoadState::Queued(_) => unreachable!(),
        }
    }
}

impl Drop for DeferredWallpaperLoad {
    fn drop(&mut self) {
        // A running file decode cannot be interrupted; it still owns its
        // permit until it exits. Queued work is dropped without spawning.
        self.cancelled.store(true, Ordering::Release);
    }
}

// Keep only the resized RGBA results, not original images. The cache owns at
// most 64 MiB / 24 entries; in-flight uploads can temporarily retain evicted
// Arc buffers. Filesystem checks and cache access happen in the decode worker,
// never in the compositor's frame loop.
const CACHE_MAX_BYTES: usize = 64 * 1024 * 1024;
const CACHE_MAX_ENTRIES: usize = 24;

#[derive(Clone, Debug, PartialEq, Eq)]
struct WallpaperCacheKey {
    path: PathBuf,
    format: image::ImageFormat,
    device: u64,
    inode: u64,
    bytes: u64,
    modified: (i64, i64),
    changed: (i64, i64),
    target: (u32, u32),
    mode: WallpaperMode,
}

impl WallpaperCacheKey {
    fn for_path(path: &Path, width: u32, height: u32, mode: WallpaperMode) -> Option<Self> {
        // The original spelling selects the decoder, just like image_source.
        // Canonical paths then allow symlink aliases to reuse the same result.
        let format = image::ImageFormat::from_path(path).ok()?;
        let path = path.canonicalize().ok()?;
        let metadata = path.metadata().ok()?;
        if !metadata.is_file() {
            return None;
        }
        Some(Self {
            path,
            format,
            device: metadata.dev(),
            inode: metadata.ino(),
            bytes: metadata.len(),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
            target: if mode == WallpaperMode::Center {
                (0, 0)
            } else {
                (width, height)
            },
            mode,
        })
    }

    fn matches_descriptor(&self, metadata: &std::fs::Metadata) -> bool {
        self.device == metadata.dev()
            && self.inode == metadata.ino()
            && self.bytes == metadata.len()
            && self.modified == (metadata.mtime(), metadata.mtime_nsec())
            && self.changed == (metadata.ctime(), metadata.ctime_nsec())
    }
}

#[derive(Default)]
struct WallpaperDecodeCache {
    entries: VecDeque<(WallpaperCacheKey, WallpaperImageData)>,
    bytes: usize,
    // Workers requesting the same source version and sampling parameters
    // share one decode. Weak references keep completed flights from becoming
    // an unbounded second cache, including images too large for the LRU.
    in_flight: Vec<(WallpaperCacheKey, Weak<WallpaperDecodeFlight>)>,
}

type WallpaperDecodeFlight = Mutex<Option<WallpaperImageData>>;

impl WallpaperDecodeCache {
    fn get(&mut self, key: &WallpaperCacheKey) -> Option<WallpaperImageData> {
        let index = self.entries.iter().position(|(entry, _)| entry == key)?;
        let entry = self.entries.remove(index)?;
        let image = entry.1.clone();
        self.entries.push_back(entry);
        Some(image)
    }

    fn flight(&mut self, key: &WallpaperCacheKey) -> Arc<WallpaperDecodeFlight> {
        self.in_flight
            .retain(|(_, flight)| flight.strong_count() > 0);
        if let Some(flight) = self
            .in_flight
            .iter()
            .find_map(|(entry, flight)| (entry == key).then(|| flight.upgrade()).flatten())
        {
            return flight;
        }
        let flight = Arc::new(Mutex::new(None));
        self.in_flight.push((key.clone(), Arc::downgrade(&flight)));
        flight
    }

    fn insert(&mut self, key: WallpaperCacheKey, image: WallpaperImageData) {
        self.insert_with_limits(key, image, CACHE_MAX_BYTES, CACHE_MAX_ENTRIES);
    }

    fn insert_with_limits(
        &mut self,
        key: WallpaperCacheKey,
        image: WallpaperImageData,
        max_bytes: usize,
        max_entries: usize,
    ) {
        // Remove obsolete versions of this file, including other sampled sizes.
        // Replacing a file in place must not retain an entire stale generation.
        self.entries.retain(|(entry, pixels)| {
            let same_source = entry.path == key.path && entry.format == key.format;
            let same_version = entry.device == key.device
                && entry.inode == key.inode
                && entry.bytes == key.bytes
                && entry.modified == key.modified
                && entry.changed == key.changed;
            let keep = !same_source || (same_version && entry != &key);
            if !keep {
                self.bytes -= pixels.rgba.len();
            }
            keep
        });
        let bytes = image.rgba.len();
        // An oversized image remains usable by its caller, but cannot evict
        // useful thumbnails only to exceed the cache's own bound.
        if bytes > max_bytes || max_entries == 0 {
            return;
        }
        while self.bytes > max_bytes - bytes || self.entries.len() >= max_entries {
            if let Some((_, old)) = self.entries.pop_front() {
                self.bytes -= old.rgba.len();
            } else {
                break;
            }
        }
        self.bytes += bytes;
        self.entries.push_back((key, image));
    }
}

fn decode_cache() -> &'static Mutex<WallpaperDecodeCache> {
    static CACHE: OnceLock<Mutex<WallpaperDecodeCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(WallpaperDecodeCache::default()))
}

/// Wallpaper surfaces are opaque. Composite transparent sources onto black
/// before resampling, so hidden RGB in PNG/WebP pixels cannot produce bright
/// fringes or show through the window shader's force-opaque path.
fn opaque_wallpaper(image: image::DynamicImage) -> image::DynamicImage {
    if !image.color().has_alpha() {
        return image;
    }
    let mut rgba = image.into_rgba8();
    for pixel in rgba.pixels_mut() {
        let alpha = u16::from(pixel[3]);
        for channel in &mut pixel.0[..3] {
            *channel = ((u16::from(*channel) * alpha + 127) / 255) as u8;
        }
        pixel[3] = 255;
    }
    image::DynamicImage::ImageRgba8(rgba)
}

fn decode_wallpaper_cached(
    path: &str,
    max_w: u32,
    max_h: u32,
    mode: WallpaperMode,
    cancelled: &AtomicBool,
    cache: &Mutex<WallpaperDecodeCache>,
) -> Option<WallpaperImageData> {
    decode_wallpaper_cached_with(path, max_w, max_h, mode, cancelled, cache, |path| {
        super::image_source::open_regular_image_with_metadata(path)
    })
}

fn decode_wallpaper_cached_with(
    path: &str,
    max_w: u32,
    max_h: u32,
    mode: WallpaperMode,
    cancelled: &AtomicBool,
    cache: &Mutex<WallpaperDecodeCache>,
    decode_source: impl FnOnce(&str) -> image::ImageResult<(image::DynamicImage, std::fs::Metadata)>,
) -> Option<WallpaperImageData> {
    if cancelled.load(Ordering::Acquire) {
        return None;
    }
    let key = WallpaperCacheKey::for_path(Path::new(path), max_w, max_h, mode);
    let flight = if let Some(key) = &key {
        let mut cache = cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(image) = cache.get(key) {
            return Some(image);
        }
        Some(cache.flight(key))
    } else {
        None
    };
    // This lock is taken only by already-bounded background workers, never
    // by the compositor. Release the cache lock before waiting, so unrelated
    // images and completed uploads continue making progress. A failed or
    // cancelled producer leaves None, letting the next live waiter retry.
    let mut shared = flight
        .as_ref()
        .map(|flight| flight.lock().unwrap_or_else(|e| e.into_inner()));
    if cancelled.load(Ordering::Acquire) {
        return None;
    }
    if let Some(Some(image)) = shared.as_deref() {
        return Some(image.clone());
    }
    let (image, source_metadata) = decode_source(path)
        .map_err(|error| log::warn!("wallpaper: failed to decode '{path}': {error}"))
        .ok()?;
    if cancelled.load(Ordering::Acquire) {
        return None;
    }
    let image = opaque_wallpaper(image);
    if cancelled.load(Ordering::Acquire) {
        return None;
    }
    let image = super::wallpaper::prepare_wallpaper_image(image, max_w, max_h, mode);
    if cancelled.load(Ordering::Acquire) {
        return None;
    }
    let rgba = image.into_rgba8();
    let image = WallpaperImageData {
        width: rgba.width(),
        height: rgba.height(),
        rgba: rgba.into_raw().into(),
        mode,
    };
    // Do not publish a result under the old identity if the file changed
    // during the bounded snapshot/decode. The caller may still use its snapshot.
    if let Some(key) = key
        && key.matches_descriptor(&source_metadata)
        && Some(&key) == WallpaperCacheKey::for_path(Path::new(path), max_w, max_h, mode).as_ref()
        && !cancelled.load(Ordering::Acquire)
    {
        cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, image.clone());
        if let Some(shared) = &mut shared {
            **shared = Some(image.clone());
        }
    }
    Some(image)
}

pub(crate) fn load_wallpaper(
    path: &str,
    max_w: u32,
    max_h: u32,
    mode: WallpaperMode,
) -> DeferredWallpaperLoad {
    let path = path.to_owned();
    DeferredWallpaperLoad::new(move |cancelled| {
        decode_wallpaper_cached(&path, max_w, max_h, mode, cancelled, decode_cache())
    })
}

#[cfg(test)]
mod cache_tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "jwm-wallpaper-cache-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn image(&self, name: &str, color: [u8; 4]) -> PathBuf {
            let path = self.0.join(name);
            image::RgbaImage::from_pixel(8, 4, image::Rgba(color))
                .save(&path)
                .unwrap();
            path
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn decode(
        path: &Path,
        target: (u32, u32),
        mode: WallpaperMode,
        cache: &Mutex<WallpaperDecodeCache>,
    ) -> Option<WallpaperImageData> {
        decode_wallpaper_cached(
            path.to_str().unwrap(),
            target.0,
            target.1,
            mode,
            &AtomicBool::new(false),
            cache,
        )
    }

    fn overlapping_decodes(
        cancel_producer: bool,
        cancel_waiter: bool,
    ) -> (
        Option<WallpaperImageData>,
        Option<WallpaperImageData>,
        usize,
    ) {
        use std::sync::atomic::AtomicUsize;
        use std::time::{Duration, Instant};

        let fixture = Fixture::new();
        let path = fixture.image("wall.png", [10, 20, 30, 255]);
        let path = path.to_str().unwrap();
        let cache = Mutex::new(WallpaperDecodeCache::default());
        let decoded = AtomicUsize::new(0);
        let producer_cancelled = AtomicBool::new(false);
        let waiter_cancelled = AtomicBool::new(false);
        let (started_tx, started_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let (producer, waiter) = std::thread::scope(|scope| {
            let cache_ref = &cache;
            let decoded_ref = &decoded;
            let cancelled = &producer_cancelled;
            let producer = scope.spawn(move || {
                decode_wallpaper_cached_with(
                    path,
                    4,
                    2,
                    WallpaperMode::Fit,
                    cancelled,
                    cache_ref,
                    |path| {
                        decoded_ref.fetch_add(1, Ordering::Relaxed);
                        let source =
                            super::super::image_source::open_regular_image_with_metadata(path);
                        started_tx.send(()).unwrap();
                        resume_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                        source
                    },
                )
            });
            started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            let waiter = scope.spawn(|| {
                decode_wallpaper_cached_with(
                    path,
                    4,
                    2,
                    WallpaperMode::Fit,
                    &waiter_cancelled,
                    &cache,
                    |path| {
                        decoded.fetch_add(1, Ordering::Relaxed);
                        super::super::image_source::open_regular_image_with_metadata(path)
                    },
                )
            });
            // Confirm both workers hold the same flight before releasing the
            // producer. This tests a real overlapping cache miss deterministically.
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if cache.lock().unwrap().in_flight[0].1.strong_count() == 2 {
                    break;
                }
                assert!(Instant::now() < deadline, "waiter did not join the decode");
                std::thread::yield_now();
            }
            producer_cancelled.store(cancel_producer, Ordering::Release);
            waiter_cancelled.store(cancel_waiter, Ordering::Release);
            resume_tx.send(()).unwrap();
            (producer.join().unwrap(), waiter.join().unwrap())
        });
        (producer, waiter, decoded.load(Ordering::Relaxed))
    }

    #[test]
    fn simultaneous_monitors_decode_and_resize_the_same_source_only_once() {
        let (first, second, decoded) = overlapping_decodes(false, false);
        let (first, second) = (first.unwrap(), second.unwrap());
        assert_eq!(decoded, 1);
        assert!(Arc::ptr_eq(&first.rgba, &second.rgba));
        assert_eq!((second.width, second.height), (4, 2));
        assert_eq!(&second.rgba[..4], &[10, 20, 30, 255]);
    }

    #[test]
    fn cancelling_a_shared_decode_waiter_keeps_the_other_monitor_alive() {
        let (producer, waiter, decoded) = overlapping_decodes(false, true);
        assert!(producer.is_some());
        assert!(waiter.is_none());
        assert_eq!(decoded, 1);
    }

    #[test]
    fn cancelling_a_shared_decode_producer_allows_a_live_monitor_to_retry() {
        let (producer, waiter, decoded) = overlapping_decodes(true, false);
        assert!(producer.is_none());
        assert!(waiter.is_some());
        assert_eq!(decoded, 2);
    }

    #[test]
    fn shared_results_can_outlive_lru_admission_without_becoming_a_second_cache() {
        let fixture = Fixture::new();
        let path = fixture.image("wall.png", [10, 20, 30, 255]);
        let key = WallpaperCacheKey::for_path(&path, 4, 2, WallpaperMode::Fit).unwrap();
        let cache = Mutex::new(WallpaperDecodeCache::default());
        let image = decode(&path, (4, 2), WallpaperMode::Fit, &cache).unwrap();
        let flight = {
            let mut cache = cache.lock().unwrap();
            // Simulate an image exceeding the LRU budget without a huge fixture.
            cache.insert_with_limits(key.clone(), image.clone(), 1, CACHE_MAX_ENTRIES);
            assert!(cache.entries.is_empty());
            assert_eq!(cache.bytes, 0);
            cache.flight(&key)
        };
        *flight.lock().unwrap() = Some(image.clone());
        let reused = decode_wallpaper_cached_with(
            path.to_str().unwrap(),
            4,
            2,
            WallpaperMode::Fit,
            &AtomicBool::new(false),
            &cache,
            |_| panic!("an active shared result needs no second decode"),
        )
        .unwrap();
        assert!(Arc::ptr_eq(&image.rgba, &reused.rgba));
        let weak = Arc::downgrade(&flight);
        drop(flight);
        assert!(weak.upgrade().is_none());
        assert!(cache.lock().unwrap().entries.is_empty());
        let fresh = decode(&path, (4, 2), WallpaperMode::Fit, &cache).unwrap();
        assert!(!Arc::ptr_eq(&image.rgba, &fresh.rgba));
        assert_eq!(cache.lock().unwrap().in_flight.len(), 1);
    }

    #[test]
    fn in_flight_identity_separates_replaced_files_and_sampling_parameters() {
        let fixture = Fixture::new();
        let path = fixture.image("wall.png", [255, 0, 0, 255]);
        let mut cache = WallpaperDecodeCache::default();
        let key = WallpaperCacheKey::for_path(&path, 4, 2, WallpaperMode::Fit).unwrap();
        let old = cache.flight(&key);
        let different_mode =
            cache.flight(&WallpaperCacheKey::for_path(&path, 4, 2, WallpaperMode::Fill).unwrap());
        let different_size =
            cache.flight(&WallpaperCacheKey::for_path(&path, 8, 4, WallpaperMode::Fit).unwrap());
        let replacement = fixture.image("new.png", [0, 0, 255, 255]);
        std::fs::rename(replacement, &path).unwrap();
        let replaced =
            cache.flight(&WallpaperCacheKey::for_path(&path, 4, 2, WallpaperMode::Fit).unwrap());
        for flight in [different_mode, different_size, replaced] {
            assert!(!Arc::ptr_eq(&old, &flight));
        }
        assert!(Arc::ptr_eq(&old, &cache.flight(&key)));
    }

    #[test]
    fn cache_reuses_pixels_across_requests_and_symlink_aliases() {
        let fixture = Fixture::new();
        let path = fixture.image("wall.png", [10, 20, 30, 255]);
        let alias = fixture.0.join("alias.png");
        std::os::unix::fs::symlink(&path, &alias).unwrap();
        let cache = Mutex::new(WallpaperDecodeCache::default());
        let first = decode(&path, (4, 2), WallpaperMode::Fit, &cache).unwrap();
        let second = decode(&alias, (4, 2), WallpaperMode::Fit, &cache).unwrap();
        assert!(Arc::ptr_eq(&first.rgba, &second.rgba));
        assert_eq!((second.width, second.height), (4, 2));
        assert_eq!(cache.lock().unwrap().bytes, 32);
        assert_eq!(cache.lock().unwrap().entries.len(), 1);
    }

    #[test]
    fn cache_is_sampling_mode_aware_and_native_center_ignores_output_size() {
        let fixture = Fixture::new();
        let path = fixture.image("wall.png", [40, 50, 60, 255]);
        let cache = Mutex::new(WallpaperDecodeCache::default());
        let fit = decode(&path, (4, 4), WallpaperMode::Fit, &cache).unwrap();
        let fill = decode(&path, (4, 4), WallpaperMode::Fill, &cache).unwrap();
        let center = decode(&path, (2, 2), WallpaperMode::Center, &cache).unwrap();
        let center_large = decode(&path, (800, 600), WallpaperMode::Center, &cache).unwrap();
        assert_eq!((fit.width, fit.height), (4, 2));
        assert_eq!((fill.width, fill.height), (8, 4));
        assert!(!Arc::ptr_eq(&fit.rgba, &fill.rgba));
        assert!(Arc::ptr_eq(&center.rgba, &center_large.rgba));
    }

    #[test]
    fn replaced_or_deleted_source_never_uses_stale_cached_pixels() {
        let fixture = Fixture::new();
        let path = fixture.image("wall.png", [255, 0, 0, 255]);
        let cache = Mutex::new(WallpaperDecodeCache::default());
        let old = decode(&path, (4, 2), WallpaperMode::Fit, &cache).unwrap();
        // Atomic replacement changes the inode even on coarse timestamp filesystems.
        let replacement = fixture.image("new.png", [0, 0, 255, 255]);
        std::fs::rename(replacement, &path).unwrap();
        let new = decode(&path, (4, 2), WallpaperMode::Fit, &cache).unwrap();
        assert!(!Arc::ptr_eq(&old.rgba, &new.rgba));
        assert_eq!(&new.rgba[..4], &[0, 0, 255, 255]);
        assert_eq!(cache.lock().unwrap().entries.len(), 1);
        std::fs::remove_file(&path).unwrap();
        assert!(decode(&path, (4, 2), WallpaperMode::Fit, &cache).is_none());
        // Eviction/invalidation cannot invalidate a result still held by an upload.
        assert_eq!(&old.rgba[..4], &[255, 0, 0, 255]);
    }

    #[test]
    fn cache_identity_is_bound_to_the_decoded_descriptor_not_a_retargeted_symlink() {
        let fixture = Fixture::new();
        let original = fixture.image("original.png", [255, 0, 0, 255]);
        let other = fixture.image("other.png", [0, 255, 0, 255]);
        let alias = fixture.0.join("alias.png");
        std::os::unix::fs::symlink(&original, &alias).unwrap();
        let key = WallpaperCacheKey::for_path(&alias, 4, 2, WallpaperMode::Fit).unwrap();
        std::fs::remove_file(&alias).unwrap();
        std::os::unix::fs::symlink(&other, &alias).unwrap();
        let (_, descriptor) =
            super::super::image_source::open_regular_image_with_metadata(&alias).unwrap();
        std::fs::remove_file(&alias).unwrap();
        std::os::unix::fs::symlink(&original, &alias).unwrap();
        // Path metadata alone again appears identical, but the decoded bytes
        // belonged to the other descriptor and must not enter this cache key.
        assert_eq!(
            Some(&key),
            WallpaperCacheKey::for_path(&alias, 4, 2, WallpaperMode::Fit).as_ref()
        );
        assert!(!key.matches_descriptor(&descriptor));
    }

    #[test]
    fn cache_rechecks_in_place_edits_and_cancellation() {
        let fixture = Fixture::new();
        let path = fixture.image("wall.png", [255, 0, 0, 255]);
        let cache = Mutex::new(WallpaperDecodeCache::default());
        let old = decode(&path, (4, 2), WallpaperMode::Fit, &cache).unwrap();
        let modified = std::time::SystemTime::now() + std::time::Duration::from_secs(2);
        image::RgbaImage::from_pixel(8, 4, image::Rgba([0, 255, 0, 255]))
            .save(&path)
            .unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(modified))
            .unwrap();
        let new = decode(&path, (4, 2), WallpaperMode::Fit, &cache).unwrap();
        assert!(!Arc::ptr_eq(&old.rgba, &new.rgba));
        assert_eq!(&new.rgba[..4], &[0, 255, 0, 255]);
        assert!(
            decode_wallpaper_cached(
                path.to_str().unwrap(),
                4,
                2,
                WallpaperMode::Fit,
                &AtomicBool::new(true),
                &cache
            )
            .is_none()
        );
    }

    #[test]
    fn lru_obeys_both_budgets_without_copying_or_invalidating_live_results() {
        let fixture = Fixture::new();
        let mut cache = WallpaperDecodeCache::default();
        let keys: Vec<_> = (0..4)
            .map(|i| {
                let path = fixture.image(&format!("{i}.png"), [0, 0, 0, 255]);
                WallpaperCacheKey::for_path(&path, 1, 1, WallpaperMode::Fit).unwrap()
            })
            .collect();
        let image = |bytes| WallpaperImageData {
            rgba: vec![7; bytes].into(),
            width: 1,
            height: 1,
            mode: WallpaperMode::Fit,
        };
        cache.insert_with_limits(keys[0].clone(), image(4), 8, 2);
        cache.insert_with_limits(keys[1].clone(), image(4), 8, 2);
        let pinned = cache.get(&keys[0]).unwrap(); // Refresh A: B must be evicted first.
        cache.insert_with_limits(keys[2].clone(), image(4), 8, 2);
        assert!(cache.get(&keys[1]).is_none());
        assert!(Arc::ptr_eq(
            &pinned.rgba,
            &cache.get(&keys[0]).unwrap().rgba
        ));
        cache.insert_with_limits(keys[3].clone(), image(9), 8, 2);
        assert_eq!(cache.bytes, 8);
        assert_eq!(cache.entries.len(), 2);
        cache.insert_with_limits(keys[3].clone(), image(4), 8, 1);
        assert_eq!(cache.entries.len(), 1);
        assert_eq!(cache.bytes, 4);
        assert_eq!(&*pinned.rgba, &[7; 4]);
    }

    #[test]
    fn transparent_hidden_rgb_is_composited_before_filtering() {
        let input = image::DynamicImage::ImageRgba8(
            image::RgbaImage::from_raw(2, 1, vec![255, 255, 255, 0, 200, 100, 50, 128]).unwrap(),
        );
        let opaque = opaque_wallpaper(input).into_rgba8();
        assert_eq!(opaque.as_raw(), &[0, 0, 0, 255, 100, 50, 25, 255]);
        let input = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            8,
            4,
            image::Rgba([255, 255, 255, 0]),
        ));
        let resized = super::super::wallpaper::prepare_wallpaper_image(
            opaque_wallpaper(input),
            2,
            2,
            WallpaperMode::Fit,
        )
        .into_rgba8();
        assert!(resized.pixels().all(|pixel| pixel.0 == [0, 0, 0, 255]));
    }
}

#[cfg(test)]
mod deferred_load_tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicUsize;

    // Leaked tiny gates give injected worker closures the same 'static permit
    // lifetime as the process-wide production gate, without shared test state.
    fn gate(permits: usize) -> &'static DecodeGate {
        Box::leak(Box::new(DecodeGate::new(permits)))
    }

    fn image(id: u32) -> WallpaperImageData {
        WallpaperImageData {
            rgba: vec![id as u8; 4].into(),
            width: id,
            height: 1,
            mode: WallpaperMode::Fit,
        }
    }

    #[test]
    fn saturated_loads_wait_in_slots_and_all_monitors_eventually_complete() {
        let gate = gate(4);
        let loads: Vec<_> = (1..=16)
            .map(|id| DeferredWallpaperLoad::new(move |_| Some(image(id))))
            .collect();
        let mut workers = VecDeque::new();
        let mut completed = vec![false; loads.len()];
        for (i, load) in loads.iter().enumerate() {
            assert!(matches!(
                load.try_recv_with(gate, |worker| {
                    workers.push_back(worker);
                    Ok(())
                }),
                Err(mpsc::TryRecvError::Empty)
            ));
            assert!(
                workers.len() <= 4,
                "request {i} must not spawn a waiting thread"
            );
        }
        assert_eq!(workers.len(), 4);
        assert_eq!(*gate.lock(), 0);
        for _ in 0..16 {
            workers.pop_front().expect("a worker can make progress")();
            for (i, load) in loads.iter().enumerate() {
                if completed[i] {
                    continue;
                }
                match load.try_recv_with(gate, |worker| {
                    workers.push_back(worker);
                    Ok(())
                }) {
                    Ok(data) => {
                        assert_eq!(data.width, i as u32 + 1);
                        assert_eq!(&*data.rgba, &[i as u8 + 1; 4]);
                        completed[i] = true;
                    }
                    Err(mpsc::TryRecvError::Empty) => {}
                    Err(mpsc::TryRecvError::Disconnected) => panic!("lost monitor {i}"),
                }
                assert!(workers.len() <= 4);
            }
        }
        assert!(completed.into_iter().all(|done| done));
        assert!(workers.is_empty());
        assert_eq!(*gate.lock(), 4);
    }

    #[test]
    fn replacing_or_dropping_a_queued_load_does_not_spawn_or_cancel_other_monitors() {
        let gate = gate(1);
        let held = gate.try_acquire().unwrap();
        let marker = Arc::new(());
        let weak = Arc::downgrade(&marker);
        let mut slot = Some(DeferredWallpaperLoad::new(move |_| {
            drop(marker);
            panic!("replaced decode ran")
        }));
        let independent = DeferredWallpaperLoad::new(|_| Some(image(2)));
        assert!(matches!(
            slot.as_ref()
                .unwrap()
                .try_recv_with(gate, |_| panic!("no permit")),
            Err(mpsc::TryRecvError::Empty)
        ));
        slot = Some(DeferredWallpaperLoad::new(|_| Some(image(3))));
        assert!(weak.upgrade().is_none());
        drop(slot);
        drop(held);
        let result = independent
            .try_recv_with(gate, |worker| {
                worker();
                Ok(())
            })
            .unwrap();
        assert_eq!(result.width, 2);
        assert_eq!(*gate.lock(), 1);
    }

    #[test]
    fn wallpaper_decode_failure_does_not_retry_from_polling_alone() {
        let gate = gate(1);
        let decoded = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&decoded);
        let load = DeferredWallpaperLoad::new(move |_| {
            count.fetch_add(1, Ordering::Relaxed);
            None
        });
        assert!(matches!(
            load.try_recv_with(gate, |worker| {
                worker();
                Ok(())
            }),
            Err(mpsc::TryRecvError::Disconnected)
        ));
        for _ in 0..10 {
            assert!(matches!(
                load.try_recv_with(gate, |_| panic!("poll must not retry")),
                Err(mpsc::TryRecvError::Disconnected)
            ));
        }
        assert_eq!(decoded.load(Ordering::Relaxed), 1);
        // Explicit refresh replaces the failed slot with a fresh request.
        let replacement = DeferredWallpaperLoad::new(|_| Some(image(7)));
        assert_eq!(
            replacement
                .try_recv_with(gate, |worker| {
                    worker();
                    Ok(())
                })
                .unwrap()
                .width,
            7
        );
    }

    #[test]
    fn wallpaper_superseded_failure_cannot_poison_current_request() {
        let gate = gate(2);
        let mut old_worker = None;
        let old = DeferredWallpaperLoad::new(|_| None);
        assert!(matches!(
            old.try_recv_with(gate, |worker| {
                old_worker = Some(worker);
                Ok(())
            }),
            Err(mpsc::TryRecvError::Empty)
        ));
        // Production drops the old receiver before installing its replacement.
        drop(old);
        let current = DeferredWallpaperLoad::new(|_| Some(image(8)));
        old_worker.take().unwrap()();
        let result = current.try_recv_with(gate, |worker| {
            worker();
            Ok(())
        });
        assert_eq!(result.unwrap().width, 8);
    }

    #[test]
    fn spawn_failure_disconnects_and_returns_the_permit() {
        let gate = gate(1);
        let load = DeferredWallpaperLoad::new(|_| panic!("failed spawn must not decode"));
        assert!(matches!(
            load.try_recv_with(gate, |_| Err(std::io::Error::other(
                "injected spawn failure"
            ))),
            Err(mpsc::TryRecvError::Disconnected)
        ));
        assert_eq!(*gate.lock(), 1);
        assert!(matches!(
            load.try_recv_with(gate, |_| panic!("failed request must not retry")),
            Err(mpsc::TryRecvError::Disconnected)
        ));
        let next = DeferredWallpaperLoad::new(|_| Some(image(4)));
        assert_eq!(
            next.try_recv_with(gate, |worker| {
                worker();
                Ok(())
            })
            .unwrap()
            .width,
            4
        );
    }

    #[test]
    fn dropping_a_started_request_cancels_only_it_and_keeps_its_permit_until_exit() {
        let gate = gate(1);
        let decoded = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&decoded);
        let load = DeferredWallpaperLoad::new(move |_| {
            count.fetch_add(1, Ordering::Relaxed);
            Some(image(1))
        });
        let mut worker = None;
        assert!(matches!(
            load.try_recv_with(gate, |job| {
                worker = Some(job);
                Ok(())
            }),
            Err(mpsc::TryRecvError::Empty)
        ));
        drop(load);
        assert_eq!(*gate.lock(), 0);
        worker.take().unwrap()();
        assert_eq!(decoded.load(Ordering::Relaxed), 0);
        assert_eq!(*gate.lock(), 1);

        let (started_tx, started_rx) = mpsc::channel();
        let (continue_tx, continue_rx) = mpsc::channel();
        let (cancel_tx, cancel_rx) = mpsc::channel();
        let load = DeferredWallpaperLoad::new(move |cancelled| {
            started_tx.send(()).unwrap();
            continue_rx.recv().unwrap();
            cancel_tx.send(cancelled.load(Ordering::Acquire)).unwrap();
            Some(image(2))
        });
        let mut thread = None;
        assert!(matches!(
            load.try_recv_with(gate, |job| {
                thread = Some(std::thread::spawn(job));
                Ok(())
            }),
            Err(mpsc::TryRecvError::Empty)
        ));
        started_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        drop(load);
        assert_eq!(*gate.lock(), 0);
        continue_tx.send(()).unwrap();
        assert!(
            cancel_rx
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap()
        );
        thread.take().unwrap().join().unwrap();
        assert_eq!(*gate.lock(), 1);
    }
}
