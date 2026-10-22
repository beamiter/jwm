//! Shared bounded, lazy wallpaper decode slots. Dropping a queued slot never spawns work.
use super::wallpaper::{WallpaperImageData, WallpaperMode};
#[path = "wallpaper_worker_pool.rs"]
mod worker_pool;
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
            .map(|n| n.get().min(worker_pool::MAX_WORKERS))
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

type WallpaperDecode = Box<dyn FnOnce(&AtomicBool) -> WallpaperDecodeOutcome + Send>;

enum WallpaperDecodeOutcome {
    Complete(Option<WallpaperImageData>),
    Waiting(Arc<WallpaperDecodeFlight>, WallpaperDecode),
}
type DecodeWorker = Box<dyn FnOnce() + Send>;

enum WallpaperLoadState {
    Queued(WallpaperDecode),
    Running(mpsc::Receiver<WallpaperDecodeOutcome>),
    Waiting(Arc<WallpaperDecodeFlight>, WallpaperDecode),
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
            state: Mutex::new(WallpaperLoadState::Queued(Box::new(move |cancelled| {
                WallpaperDecodeOutcome::Complete(decode(cancelled))
            }))),
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn try_recv(&self) -> Result<WallpaperImageData, mpsc::TryRecvError> {
        self.try_recv_from_pool(worker_pool::get_or_start())
    }

    fn try_recv_from_pool(
        &self,
        pool: std::io::Result<&worker_pool::WorkerPool>,
    ) -> Result<WallpaperImageData, mpsc::TryRecvError> {
        match pool {
            Ok(pool) => self.try_recv_with(decode_gate(), |worker| pool.submit(worker)),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                // Another caller is starting the pool. Keep the queued task
                // and its cancellation identity intact until the next poll.
                Err(mpsc::TryRecvError::Empty)
            }
            Err(error) => self.try_recv_with(decode_gate(), |_| Err(error)),
        }
    }

    fn try_recv_with(
        &self,
        gate: &'static DecodeGate,
        spawn: impl FnOnce(DecodeWorker) -> std::io::Result<()>,
    ) -> Result<WallpaperImageData, mpsc::TryRecvError> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let WallpaperLoadState::Waiting(flight, _) = &*state {
            // Only inspect an in-memory lock; pathname identity is rechecked
            // by the next bounded worker, never on the compositor thread.
            if matches!(flight.try_lock(), Err(std::sync::TryLockError::WouldBlock)) {
                return Err(mpsc::TryRecvError::Empty);
            }
            let WallpaperLoadState::Waiting(flight, decode) =
                std::mem::replace(&mut *state, WallpaperLoadState::Failed)
            else {
                unreachable!();
            };
            *state = WallpaperLoadState::Queued(Box::new(move |cancelled| {
                // Keep oversized (non-LRU) results alive across queueing and
                // the worker's source-identity lookup, not just while waiting.
                let _flight = flight;
                decode(cancelled)
            }));
        }
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
                if !cancelled.load(Ordering::Acquire) {
                    let result = decode(&cancelled);
                    if !cancelled.load(Ordering::Acquire) {
                        let _ = tx.send(result);
                    }
                }
            });
            match spawn(worker) {
                Ok(()) => *state = WallpaperLoadState::Running(rx),
                Err(error) => log::warn!("compositor: could not start wallpaper decoder: {error}"),
            }
        }
        match &*state {
            WallpaperLoadState::Running(rx) => match rx.try_recv() {
                Ok(WallpaperDecodeOutcome::Complete(Some(image))) => Ok(image),
                Ok(WallpaperDecodeOutcome::Complete(None)) => {
                    *state = WallpaperLoadState::Failed;
                    Err(mpsc::TryRecvError::Disconnected)
                }
                Ok(WallpaperDecodeOutcome::Waiting(flight, decode)) => {
                    *state = WallpaperLoadState::Waiting(flight, decode);
                    Err(mpsc::TryRecvError::Empty)
                }
                Err(error) => Err(error),
            },
            WallpaperLoadState::Waiting(_, _) => Err(mpsc::TryRecvError::Empty),
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
        if alpha == 255 {
            continue;
        }
        if alpha == 0 {
            pixel.0 = [0, 0, 0, 255];
            continue;
        }
        for channel in &mut pixel.0[..3] {
            *channel = ((u16::from(*channel) * alpha + 127) / 255) as u8;
        }
        pixel[3] = 255;
    }
    image::DynamicImage::ImageRgba8(rgba)
}

#[cfg(test)]
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

#[cfg(test)]
fn decode_wallpaper_cached_with(
    path: &str,
    max_w: u32,
    max_h: u32,
    mode: WallpaperMode,
    cancelled: &AtomicBool,
    cache: &Mutex<WallpaperDecodeCache>,
    decode_source: impl FnOnce(&str) -> image::ImageResult<(image::DynamicImage, std::fs::Metadata)>,
) -> Option<WallpaperImageData> {
    decode_wallpaper_cached_attempt(
        path,
        (max_w, max_h, mode),
        cancelled,
        cache,
        decode_source,
        false,
    )
    .ok()
    .flatten()
}

fn decode_wallpaper_cached_attempt(
    path: &str,
    sampling: (u32, u32, WallpaperMode),
    cancelled: &AtomicBool,
    cache: &Mutex<WallpaperDecodeCache>,
    decode_source: impl FnOnce(&str) -> image::ImageResult<(image::DynamicImage, std::fs::Metadata)>,
    defer_busy: bool,
) -> Result<Option<WallpaperImageData>, Arc<WallpaperDecodeFlight>> {
    let (max_w, max_h, mode) = sampling;
    if cancelled.load(Ordering::Acquire) {
        return Ok(None);
    }
    let key = WallpaperCacheKey::for_path(Path::new(path), max_w, max_h, mode);
    let flight = if let Some(key) = &key {
        let mut cache = cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(image) = cache.get(key) {
            return Ok(Some(image));
        }
        Some(cache.flight(key))
    } else {
        None
    };
    // Production workers never wait on a producer: they return its flight
    // to the deferred slot and release their permit. Synchronous test callers
    // may wait here. A failed/cancelled producer leaves None for a live retry.
    let mut shared = if let Some(flight) = &flight {
        Some(if defer_busy {
            match flight.try_lock() {
                Ok(shared) => shared,
                Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
                Err(std::sync::TryLockError::WouldBlock) => return Err(Arc::clone(flight)),
            }
        } else {
            flight.lock().unwrap_or_else(|e| e.into_inner())
        })
    } else {
        None
    };
    if cancelled.load(Ordering::Acquire) {
        return Ok(None);
    }
    if let Some(Some(image)) = shared.as_deref() {
        return Ok(Some(image.clone()));
    }
    let (image, source_metadata) = match decode_source(path) {
        Ok(source) => source,
        Err(error) => {
            log::warn!("wallpaper: failed to decode '{path}': {error}");
            return Ok(None);
        }
    };
    if cancelled.load(Ordering::Acquire) {
        return Ok(None);
    }
    let image = opaque_wallpaper(image);
    if cancelled.load(Ordering::Acquire) {
        return Ok(None);
    }
    let image = super::wallpaper::prepare_wallpaper_image(image, max_w, max_h, mode);
    if cancelled.load(Ordering::Acquire) {
        return Ok(None);
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
    Ok(Some(image))
}

pub(crate) fn load_wallpaper(
    path: &str,
    max_w: u32,
    max_h: u32,
    mode: WallpaperMode,
) -> DeferredWallpaperLoad {
    DeferredWallpaperLoad {
        state: Mutex::new(WallpaperLoadState::Queued(wallpaper_decode_task(
            path.to_owned(),
            max_w,
            max_h,
            mode,
        ))),
        cancelled: Arc::new(AtomicBool::new(false)),
    }
}

fn wallpaper_decode_task(
    path: String,
    max_w: u32,
    max_h: u32,
    mode: WallpaperMode,
) -> WallpaperDecode {
    Box::new(move |cancelled| {
        // Resolve the configured spelling only in the bounded worker. Keep
        // the original path in the retry closure and compositor request so
        // overrides/deduplication retain their existing identity semantics.
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
        match decode_wallpaper_for_home(&path, (max_w, max_h, mode), cancelled, &home) {
            Ok(image) => WallpaperDecodeOutcome::Complete(image),
            Err(flight) => WallpaperDecodeOutcome::Waiting(
                flight,
                wallpaper_decode_task(path, max_w, max_h, mode),
            ),
        }
    })
}

fn decode_wallpaper_for_home(
    path: &str,
    sampling: (u32, u32, WallpaperMode),
    cancelled: &AtomicBool,
    home: &Path,
) -> Result<Option<WallpaperImageData>, Arc<WallpaperDecodeFlight>> {
    let resolved = crate::jwm::features::wallpaper::expand_home(path, home);
    let Some(path) = resolved.to_str() else {
        log::warn!("wallpaper: resolved path is not UTF-8: {resolved:?}");
        return Ok(None);
    };
    // Cache identity, format selection and descriptor opening must all use
    // this same resolved pathname, including after a shared-flight retry.
    decode_wallpaper_cached_attempt(
        path,
        sampling,
        cancelled,
        decode_cache(),
        |path| super::image_source::open_regular_image_with_metadata(path),
        true,
    )
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
    fn worker_expands_home_and_shares_cache_identity_with_absolute_paths() {
        let fixture = Fixture::new();
        let path = fixture.image("home.png", [10, 20, 30, 255]);
        let gate = Box::leak(Box::new(DecodeGate::new(1)));
        let caller = std::thread::current().id();
        let decode = |configured: &str| {
            let configured = configured.to_owned();
            let home = fixture.0.clone();
            let load = DeferredWallpaperLoad::new(move |cancelled| {
                assert_ne!(
                    std::thread::current().id(),
                    caller,
                    "file access belongs in the worker"
                );
                decode_wallpaper_for_home(&configured, (4, 2, WallpaperMode::Fit), cancelled, &home)
                    .ok()
                    .flatten()
            });
            load.try_recv_with(gate, |worker| {
                std::thread::spawn(worker).join().unwrap();
                Ok(())
            })
            .unwrap()
        };
        let expanded = decode("~/home.png");
        let absolute = decode(path.to_str().unwrap());
        assert!(Arc::ptr_eq(&expanded.rgba, &absolute.rgba));
        let replacement = fixture.image("replacement.png", [70, 80, 90, 255]);
        std::fs::rename(replacement, &path).unwrap();
        let refreshed = decode("~/home.png");
        assert_eq!(&refreshed.rgba[..4], &[70, 80, 90, 255]);
        assert!(!Arc::ptr_eq(&expanded.rgba, &refreshed.rgba));
        let absolute = decode(path.to_str().unwrap());
        assert!(Arc::ptr_eq(&refreshed.rgba, &absolute.rgba));
    }

    #[test]
    fn shared_home_resolver_preserves_relative_absolute_and_named_user_paths() {
        use crate::jwm::features::wallpaper::expand_home;
        let home = Path::new("/fake/home");
        for (configured, expected) in [
            ("~", "/fake/home"),
            ("~/wall.png", "/fake/home/wall.png"),
            ("relative/wall.png", "relative/wall.png"),
            ("/absolute/wall.png", "/absolute/wall.png"),
            ("~someone/wall.png", "~someone/wall.png"),
            ("$HOME/wall.png", "$HOME/wall.png"),
        ] {
            assert_eq!(expand_home(configured, home), Path::new(expected));
        }
    }

    #[test]
    fn opaque_fast_paths_match_every_alpha_and_preserve_the_rgba_allocation() {
        let mut pixels = image::RgbaImage::from_fn(256, 256, |channel, alpha| {
            image::Rgba([
                channel as u8,
                255 - channel as u8,
                (channel / 2) as u8,
                alpha as u8,
            ])
        });
        let original = pixels.clone();
        let allocation = pixels.as_mut_ptr();
        let opaque = opaque_wallpaper(image::DynamicImage::ImageRgba8(pixels)).into_rgba8();
        assert_eq!(opaque.as_ptr(), allocation);
        for (source, actual) in original.pixels().zip(opaque.pixels()) {
            for channel in 0..3 {
                let expected =
                    ((u16::from(source[channel]) * u16::from(source[3]) + 127) / 255) as u8;
                assert_eq!(actual[channel], expected);
            }
            assert_eq!(actual[3], 255);
        }
    }

    #[test]
    fn shared_waiters_release_gate_and_reuse_non_lru_result_without_waiting_threads() {
        let fixture = Fixture::new();
        let path = fixture.image("flight.png", [10, 20, 30, 255]);
        let key = WallpaperCacheKey::for_path(&path, 4, 2, WallpaperMode::Fit).unwrap();
        let flight = decode_cache().lock().unwrap().flight(&key);
        let mut producer = flight.lock().unwrap();
        let gate = Box::leak(Box::new(DecodeGate::new(2)));
        let producer_permit = gate.try_acquire().unwrap();
        let loads: Vec<_> = (0..8)
            .map(|_| load_wallpaper(path.to_str().unwrap(), 4, 2, WallpaperMode::Fit))
            .collect();
        for load in &loads {
            assert!(matches!(
                load.try_recv_with(gate, |worker| {
                    worker();
                    Ok(())
                }),
                Err(mpsc::TryRecvError::Empty)
            ));
            assert_eq!(*gate.lock(), 1, "waiting worker must release its permit");
            for _ in 0..3 {
                assert!(matches!(
                    load.try_recv_with(gate, |_| panic!("busy flight must not spawn")),
                    Err(mpsc::TryRecvError::Empty)
                ));
            }
        }
        let unrelated = DeferredWallpaperLoad::new(|_| {
            Some(WallpaperImageData {
                width: 1,
                height: 1,
                rgba: vec![1; 4].into(),
                mode: WallpaperMode::Fit,
            })
        });
        assert!(
            unrelated
                .try_recv_with(gate, |worker| {
                    worker();
                    Ok(())
                })
                .is_ok()
        );
        // Deliberately publish only to the flight, reproducing an image that
        // exceeds the LRU bound without allocating an oversized test buffer.
        let pixels: Arc<[u8]> = vec![42; 4 * 2 * 4].into();
        *producer = Some(WallpaperImageData {
            width: 4,
            height: 2,
            rgba: Arc::clone(&pixels),
            mode: WallpaperMode::Fit,
        });
        drop(producer);
        drop(flight);
        drop(producer_permit);
        for load in loads {
            let result = load
                .try_recv_with(gate, |worker| {
                    worker();
                    Ok(())
                })
                .unwrap();
            assert!(
                Arc::ptr_eq(&pixels, &result.rgba),
                "non-LRU flight must survive requeue"
            );
        }
    }

    #[test]
    fn deferred_waiter_rechecks_identity_after_its_producer_finishes() {
        let fixture = Fixture::new();
        let path = fixture.image("changed.png", [10, 20, 30, 255]);
        let key = WallpaperCacheKey::for_path(&path, 4, 2, WallpaperMode::Fit).unwrap();
        let flight = decode_cache().lock().unwrap().flight(&key);
        let mut producer = flight.lock().unwrap();
        let gate = Box::leak(Box::new(DecodeGate::new(1)));
        let load = load_wallpaper(path.to_str().unwrap(), 4, 2, WallpaperMode::Fit);
        assert!(matches!(
            load.try_recv_with(gate, |worker| {
                worker();
                Ok(())
            }),
            Err(mpsc::TryRecvError::Empty)
        ));
        *producer = Some(WallpaperImageData {
            width: 4,
            height: 2,
            rgba: vec![42; 32].into(),
            mode: WallpaperMode::Fit,
        });
        let replacement = fixture.image("replacement.png", [70, 80, 90, 255]);
        std::fs::rename(replacement, &path).unwrap();
        drop(producer);
        let result = load
            .try_recv_with(gate, |worker| {
                worker();
                Ok(())
            })
            .unwrap();
        assert_eq!(&result.rgba[..4], &[70, 80, 90, 255]);
    }

    #[test]
    fn deferred_waiter_takes_over_an_empty_failed_or_cancelled_flight() {
        let fixture = Fixture::new();
        let path = fixture.image("retry.png", [10, 20, 30, 255]);
        let key = WallpaperCacheKey::for_path(&path, 4, 2, WallpaperMode::Fit).unwrap();
        let flight = decode_cache().lock().unwrap().flight(&key);
        let producer = flight.lock().unwrap();
        let gate = Box::leak(Box::new(DecodeGate::new(1)));
        let load = load_wallpaper(path.to_str().unwrap(), 4, 2, WallpaperMode::Fit);
        assert!(matches!(
            load.try_recv_with(gate, |worker| {
                worker();
                Ok(())
            }),
            Err(mpsc::TryRecvError::Empty)
        ));
        drop(producer); // Both cancellation and decode failure leave no result.
        let result = load
            .try_recv_with(gate, |worker| {
                worker();
                Ok(())
            })
            .unwrap();
        assert_eq!(&result.rgba[..4], &[10, 20, 30, 255]);
    }

    #[test]
    fn production_slot_cancellation_and_spawn_failure_release_permits_for_a_live_request() {
        let fixture = Fixture::new();
        let path = fixture.image("cancelled.png", [10, 20, 30, 255]);
        let gate = Box::leak(Box::new(DecodeGate::new(1)));
        let load = || load_wallpaper(path.to_str().unwrap(), 4, 2, WallpaperMode::Fit);
        let producer = load();
        let mut queued_worker = None;
        assert!(matches!(
            producer.try_recv_with(gate, |worker| {
                queued_worker = Some(worker);
                Ok(())
            }),
            Err(mpsc::TryRecvError::Empty)
        ));
        drop(producer);
        assert_eq!(*gate.lock(), 0);
        queued_worker.take().unwrap()();
        assert_eq!(*gate.lock(), 1);
        let failed = load();
        assert!(matches!(
            failed.try_recv_with(gate, |_| Err(std::io::Error::other("spawn rejected"))),
            Err(mpsc::TryRecvError::Disconnected)
        ));
        assert_eq!(*gate.lock(), 1);
        assert!(matches!(
            failed.try_recv_with(gate, |_| panic!("failed slot must not retry")),
            Err(mpsc::TryRecvError::Disconnected)
        ));
        let result = load()
            .try_recv_with(gate, |worker| {
                worker();
                Ok(())
            })
            .unwrap();
        assert_eq!(&result.rgba[..4], &[10, 20, 30, 255]);
    }

    #[test]
    fn cancelled_waiter_and_failed_resume_do_not_cancel_a_live_flight() {
        let fixture = Fixture::new();
        let path = fixture.image("cancel-waiter.png", [10, 20, 30, 255]);
        let key = WallpaperCacheKey::for_path(&path, 4, 2, WallpaperMode::Fit).unwrap();
        let flight = decode_cache().lock().unwrap().flight(&key);
        let mut producer = flight.lock().unwrap();
        let gate = Box::leak(Box::new(DecodeGate::new(1)));
        let load = || load_wallpaper(path.to_str().unwrap(), 4, 2, WallpaperMode::Fit);
        let cancelled = load();
        let failed = load();
        let live = load();
        for waiter in [&cancelled, &failed, &live] {
            assert!(matches!(
                waiter.try_recv_with(gate, |worker| {
                    worker();
                    Ok(())
                }),
                Err(mpsc::TryRecvError::Empty)
            ));
        }
        assert_eq!(Arc::strong_count(&flight), 4);
        drop(cancelled);
        assert_eq!(Arc::strong_count(&flight), 3);
        let pixels: Arc<[u8]> = vec![42; 32].into();
        *producer = Some(WallpaperImageData {
            width: 4,
            height: 2,
            rgba: Arc::clone(&pixels),
            mode: WallpaperMode::Fit,
        });
        drop(producer);
        assert!(matches!(
            failed.try_recv_with(gate, |_| Err(std::io::Error::other("resume rejected"))),
            Err(mpsc::TryRecvError::Disconnected)
        ));
        assert_eq!(*gate.lock(), 1);
        assert_eq!(Arc::strong_count(&flight), 2);
        let result = live
            .try_recv_with(gate, |worker| {
                worker();
                Ok(())
            })
            .unwrap();
        assert!(Arc::ptr_eq(&pixels, &result.rgba));
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
    fn pool_initialization_contention_preserves_the_queued_request() {
        let count = Arc::new(AtomicUsize::new(0));
        let decoded = Arc::clone(&count);
        let load = DeferredWallpaperLoad::new(move |_| {
            decoded.fetch_add(1, Ordering::Relaxed);
            Some(image(5))
        });
        for _ in 0..3 {
            assert!(matches!(
                load.try_recv_from_pool(Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "initializing"
                ))),
                Err(mpsc::TryRecvError::Empty)
            ));
        }
        assert_eq!(count.load(Ordering::Relaxed), 0);
        let gate = gate(1);
        assert_eq!(
            load.try_recv_with(gate, |worker| {
                worker();
                Ok(())
            })
            .unwrap()
            .width,
            5
        );
        assert_eq!(count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn every_gate_permitted_job_fits_the_pool_without_a_running_consumer() {
        let mut workers = Vec::new();
        let pool = worker_pool::WorkerPool::start_with(|worker| {
            workers.push(worker);
            Ok(())
        })
        .unwrap();
        let gate = gate(worker_pool::MAX_WORKERS);
        let loads: Vec<_> = (0..worker_pool::MAX_WORKERS * 2)
            .map(|_| DeferredWallpaperLoad::new(|_| panic!("cancelled queue must not decode")))
            .collect();
        let mut submitted = 0;
        for load in &loads {
            assert!(matches!(
                load.try_recv_with(gate, |worker| {
                    submitted += 1;
                    pool.submit(worker)
                }),
                Err(mpsc::TryRecvError::Empty)
            ));
        }
        assert_eq!(submitted, worker_pool::MAX_WORKERS);
        assert_eq!(*gate.lock(), 0);
        drop(loads);
        drop(pool);
        for worker in workers {
            worker();
        }
        assert_eq!(*gate.lock(), worker_pool::MAX_WORKERS);
    }

    #[test]
    fn pooled_cancelled_queue_skips_decode_and_panics_release_permits() {
        use std::time::Duration;
        let mut started = false;
        let pool = worker_pool::WorkerPool::start_with(|worker| {
            if started {
                return Err(std::io::Error::other("one test worker"));
            }
            started = true;
            std::thread::spawn(worker);
            Ok(())
        })
        .unwrap();
        let gate = gate(1);
        let (started_tx, started_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        pool.submit(Box::new(move || {
            started_tx.send(()).unwrap();
            resume_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        }))
        .unwrap();
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let decoded = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&decoded);
        let cancelled = DeferredWallpaperLoad::new(move |_| {
            count.fetch_add(1, Ordering::Relaxed);
            Some(image(1))
        });
        assert!(matches!(
            cancelled.try_recv_with(gate, |job| pool.submit(job)),
            Err(mpsc::TryRecvError::Empty)
        ));
        drop(cancelled);
        assert_eq!(
            *gate.lock(),
            0,
            "queued job retains its permit until drained"
        );
        resume_tx.send(()).unwrap();
        let flush = || {
            let (tx, rx) = mpsc::channel();
            pool.submit(Box::new(move || {
                tx.send(()).unwrap();
            }))
            .unwrap();
            rx.recv_timeout(Duration::from_secs(5)).unwrap();
        };
        flush();
        assert_eq!(decoded.load(Ordering::Relaxed), 0);
        assert_eq!(*gate.lock(), 1);
        let panicking = DeferredWallpaperLoad::new(|_| panic!("decoder panic"));
        let _ = panicking.try_recv_with(gate, |job| pool.submit(job));
        flush();
        assert!(matches!(
            panicking.try_recv_with(gate, |_| panic!("dead job must not retry")),
            Err(mpsc::TryRecvError::Disconnected)
        ));
        assert_eq!(*gate.lock(), 1);
        let live = DeferredWallpaperLoad::new(|_| Some(image(7)));
        let first = live.try_recv_with(gate, |job| pool.submit(job));
        flush();
        let result = first.or_else(|_| live.try_recv_with(gate, |_| panic!("already submitted")));
        assert_eq!(result.unwrap().width, 7);
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
