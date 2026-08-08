//! Backend-neutral policy and storage for bounded minimized-window snapshots.
//!
//! `IconicPinned` is granted only by an explicit backend admission boundary:
//! ordinary captures remain recapturable until a backend has verified the
//! exact CPU generation it will retain after the client becomes unmapped.

use std::collections::HashMap;
use std::fmt;
use std::hash::Hash;
use std::sync::Arc;

/// Maximum stored snapshot extent. Snapshots never upscale their source.
pub(crate) const SNAPSHOT_MAX_WIDTH: u32 = 256;
pub(crate) const SNAPSHOT_MAX_HEIGHT: u32 = 192;
pub(crate) const SNAPSHOT_CHANNELS: usize = 4;

/// Independent CPU budget for Dock-quality snapshots.
pub(crate) const SNAPSHOT_CACHE_MAX_BYTES: usize = 24 * 1024 * 1024;
pub(crate) const SNAPSHOT_CACHE_MAX_ENTRIES: usize = 128;

/// Fragment stage for the phase-two capture path. Four samples along each
/// axis cover the complete destination-pixel footprint, avoiding the severe
/// aliasing of a single bilinear lookup when a 4K client is reduced to a Dock
/// card. Wayland can link this constant directly; X11 requests the desktop
/// dialect through `thumbnail_downsample_fragment_shader`.
pub(crate) const THUMBNAIL_DOWNSAMPLE_TAPS: usize = 16;
pub(crate) const THUMBNAIL_DOWNSAMPLE_FRAGMENT_SHADER: &str = r#"#version 300 es
precision highp float;

uniform sampler2D u_texture;
uniform vec4 u_uv_rect;
uniform vec2 u_output_size;
uniform int u_has_alpha;
in vec2 v_uv;
out vec4 frag_color;

vec4 sample_clamped(vec2 uv, vec2 uv_min, vec2 uv_max) {
    return texture(u_texture, clamp(uv, uv_min, uv_max));
}

void main() {
    vec2 uv = u_uv_rect.xy + v_uv * u_uv_rect.zw;
    vec2 footprint = abs(u_uv_rect.zw) / max(u_output_size, vec2(1.0));
    vec2 dx = vec2(footprint.x * 0.125, 0.0);
    vec2 dy = vec2(0.0, footprint.y * 0.125);
    vec2 uv_min = min(u_uv_rect.xy, u_uv_rect.xy + u_uv_rect.zw);
    vec2 uv_max = max(u_uv_rect.xy, u_uv_rect.xy + u_uv_rect.zw);

    vec4 color =
          sample_clamped(uv - 3.0 * dx - 3.0 * dy, uv_min, uv_max)
        + sample_clamped(uv -       dx - 3.0 * dy, uv_min, uv_max)
        + sample_clamped(uv +       dx - 3.0 * dy, uv_min, uv_max)
        + sample_clamped(uv + 3.0 * dx - 3.0 * dy, uv_min, uv_max)
        + sample_clamped(uv - 3.0 * dx -       dy, uv_min, uv_max)
        + sample_clamped(uv -       dx -       dy, uv_min, uv_max)
        + sample_clamped(uv +       dx -       dy, uv_min, uv_max)
        + sample_clamped(uv + 3.0 * dx -       dy, uv_min, uv_max)
        + sample_clamped(uv - 3.0 * dx +       dy, uv_min, uv_max)
        + sample_clamped(uv -       dx +       dy, uv_min, uv_max)
        + sample_clamped(uv +       dx +       dy, uv_min, uv_max)
        + sample_clamped(uv + 3.0 * dx +       dy, uv_min, uv_max)
        + sample_clamped(uv - 3.0 * dx + 3.0 * dy, uv_min, uv_max)
        + sample_clamped(uv -       dx + 3.0 * dy, uv_min, uv_max)
        + sample_clamped(uv +       dx + 3.0 * dy, uv_min, uv_max)
        + sample_clamped(uv + 3.0 * dx + 3.0 * dy, uv_min, uv_max);
    color *= 1.0 / 16.0;
    if (u_has_alpha == 0) {
        color.a = 1.0;
    }
    frag_color = color;
}
"#;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ThumbnailShaderDialect {
    Gles300,
    Gl330,
}

pub(crate) fn thumbnail_downsample_fragment_shader(
    dialect: ThumbnailShaderDialect,
) -> std::borrow::Cow<'static, str> {
    match dialect {
        ThumbnailShaderDialect::Gles300 => {
            std::borrow::Cow::Borrowed(THUMBNAIL_DOWNSAMPLE_FRAGMENT_SHADER)
        }
        ThumbnailShaderDialect::Gl330 => {
            std::borrow::Cow::Owned(THUMBNAIL_DOWNSAMPLE_FRAGMENT_SHADER.replacen(
                "#version 300 es\nprecision highp float;",
                "#version 330 core",
                1,
            ))
        }
    }
}

/// The shared window shader uses the sign of opacity to select texture alpha.
/// Opaque imports must force alpha to one because XRGB padding is undefined.
pub(crate) const fn snapshot_shader_opacity(has_alpha: bool) -> f32 {
    if has_alpha { -1.0 } else { 1.0 }
}

/// Monotonic capture generation used to reject out-of-order async results.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct SnapshotGeneration(u64);

impl SnapshotGeneration {
    pub(crate) const fn new(value: u64) -> Option<Self> {
        if value == 0 { None } else { Some(Self(value)) }
    }

    pub(crate) const fn get(self) -> u64 {
        self.0
    }
}

/// Validation failure for the fixed, tightly packed top-left RGBA8 contract.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SnapshotError {
    ZeroDimension,
    WidthExceedsLimit,
    HeightExceedsLimit,
    InvalidGeneration,
    ByteLengthMismatch { expected: usize, actual: usize },
}

/// A tightly packed RGBA8 snapshot whose first row is the image's top row.
#[derive(Clone)]
pub(crate) struct MinimizedSnapshot {
    width: u32,
    height: u32,
    generation: SnapshotGeneration,
    /// Source-window alpha semantics, independent of the RGBA8 storage.
    has_alpha: bool,
    rgba: Arc<[u8]>,
}

impl MinimizedSnapshot {
    pub(crate) fn try_new(
        width: u32,
        height: u32,
        generation: u64,
        has_alpha: bool,
        rgba: impl Into<Arc<[u8]>>,
    ) -> Result<Self, SnapshotError> {
        if width == 0 || height == 0 {
            return Err(SnapshotError::ZeroDimension);
        }
        if width > SNAPSHOT_MAX_WIDTH {
            return Err(SnapshotError::WidthExceedsLimit);
        }
        if height > SNAPSHOT_MAX_HEIGHT {
            return Err(SnapshotError::HeightExceedsLimit);
        }
        let generation =
            SnapshotGeneration::new(generation).ok_or(SnapshotError::InvalidGeneration)?;
        let expected = width as usize * height as usize * SNAPSHOT_CHANNELS;
        let rgba = rgba.into();
        if rgba.len() != expected {
            return Err(SnapshotError::ByteLengthMismatch {
                expected,
                actual: rgba.len(),
            });
        }

        Ok(Self {
            width,
            height,
            generation,
            has_alpha,
            rgba,
        })
    }

    pub(crate) const fn width(&self) -> u32 {
        self.width
    }

    pub(crate) const fn height(&self) -> u32 {
        self.height
    }

    pub(crate) const fn generation(&self) -> SnapshotGeneration {
        self.generation
    }

    pub(crate) const fn has_alpha(&self) -> bool {
        self.has_alpha
    }

    pub(crate) fn rgba(&self) -> &Arc<[u8]> {
        &self.rgba
    }

    pub(crate) fn byte_len(&self) -> usize {
        self.rgba.len()
    }
}

impl fmt::Debug for MinimizedSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MinimizedSnapshot")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("generation", &self.generation)
            .field("has_alpha", &self.has_alpha)
            .field("byte_len", &self.rgba.len())
            .finish()
    }
}

/// Fit within the snapshot envelope without upscaling, using integer math.
pub(crate) const fn snapshot_dimensions(
    source_width: u32,
    source_height: u32,
) -> Option<(u32, u32)> {
    if source_width == 0 || source_height == 0 {
        return None;
    }
    if source_width <= SNAPSHOT_MAX_WIDTH && source_height <= SNAPSHOT_MAX_HEIGHT {
        return Some((source_width, source_height));
    }

    let width_limited = source_width as u64 * SNAPSHOT_MAX_HEIGHT as u64
        >= source_height as u64 * SNAPSHOT_MAX_WIDTH as u64;
    if width_limited {
        let scaled = source_height as u64 * SNAPSHOT_MAX_WIDTH as u64 / source_width as u64;
        let height = if scaled == 0 { 1 } else { scaled as u32 };
        Some((SNAPSHOT_MAX_WIDTH, height))
    } else {
        let scaled = source_width as u64 * SNAPSHOT_MAX_HEIGHT as u64 / source_height as u64;
        let width = if scaled == 0 { 1 } else { scaled as u32 };
        Some((width, SNAPSHOT_MAX_HEIGHT))
    }
}

/// Whether an entry can be recreated while its client remains mapped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SnapshotRetention {
    /// Safe LRU victim: the mapped hidden surface can provide another buffer.
    RecapturableMapped,
    /// Never an LRU victim: an actually iconified client has no capture source.
    IconicPinned,
}

/// Snapshot/display sources considered by Dock and restore call sites.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ThumbnailSource {
    /// The ordinary mapped client texture; always full resolution.
    LiveMappedTexture,
    /// The full-resolution texture retained for reverse Genie animation.
    RetainedVisual,
    /// A GPU upload of the bounded snapshot, optimal for a static Dock card.
    GpuSnapshot,
    /// The bounded CPU RGBA snapshot represented by this module.
    CpuSnapshot,
    /// App icon or generic fallback owned by the bar.
    Placeholder,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ThumbnailPurpose {
    StaticDockCard,
    HoverPreview,
    RestoreAnimation,
}

/// Return a larger number for a preferred source, or `None` when using that
/// source would violate the consumer's quality contract.
///
/// In particular, a CPU snapshot is never upscaled into reverse Genie. The
/// retained visual wins for restore because it is the exact minimize frame.
pub(crate) const fn thumbnail_source_priority(
    purpose: ThumbnailPurpose,
    source: ThumbnailSource,
) -> Option<u8> {
    match (purpose, source) {
        (ThumbnailPurpose::StaticDockCard, ThumbnailSource::GpuSnapshot) => Some(5),
        (ThumbnailPurpose::StaticDockCard, ThumbnailSource::RetainedVisual) => Some(4),
        (ThumbnailPurpose::StaticDockCard, ThumbnailSource::LiveMappedTexture) => Some(3),
        (ThumbnailPurpose::StaticDockCard, ThumbnailSource::CpuSnapshot) => Some(2),
        (ThumbnailPurpose::StaticDockCard, ThumbnailSource::Placeholder) => Some(1),
        (ThumbnailPurpose::HoverPreview, ThumbnailSource::RetainedVisual) => Some(5),
        (ThumbnailPurpose::HoverPreview, ThumbnailSource::LiveMappedTexture) => Some(4),
        (ThumbnailPurpose::HoverPreview, ThumbnailSource::GpuSnapshot) => Some(3),
        (ThumbnailPurpose::HoverPreview, ThumbnailSource::CpuSnapshot) => Some(2),
        (ThumbnailPurpose::HoverPreview, ThumbnailSource::Placeholder) => Some(1),
        (ThumbnailPurpose::RestoreAnimation, ThumbnailSource::RetainedVisual) => Some(2),
        (ThumbnailPurpose::RestoreAnimation, ThumbnailSource::LiveMappedTexture) => Some(1),
        (
            ThumbnailPurpose::RestoreAnimation,
            ThumbnailSource::GpuSnapshot
            | ThumbnailSource::CpuSnapshot
            | ThumbnailSource::Placeholder,
        ) => None,
    }
}

/// Pick the highest-quality admissible source independently of input order.
pub(crate) fn preferred_thumbnail_source(
    purpose: ThumbnailPurpose,
    available: impl IntoIterator<Item = ThumbnailSource>,
) -> Option<ThumbnailSource> {
    available
        .into_iter()
        .filter_map(|source| thumbnail_source_priority(purpose, source).map(|rank| (rank, source)))
        .max_by_key(|(rank, _)| *rank)
        .map(|(_, source)| source)
}

#[derive(Debug)]
struct CacheEntry {
    snapshot: MinimizedSnapshot,
    retention: SnapshotRetention,
    last_use: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum AdmissionOutcome<K> {
    Admitted {
        evicted: Vec<K>,
    },
    AlreadyCurrent,
    RejectedStale,
    /// The proposed value cannot fit without replacing an iconic reservation
    /// or evicting another pinned entry. The cache is unchanged.
    RejectedCapacity,
}

/// Why the current capture could not be reserved across an Iconic transition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum IconicSnapshotReservationError {
    /// The cache has no CPU pixels for the requested window.
    NoSnapshot,
    /// CPU pixels exist, but belong to a different capture epoch.
    GenerationMismatch {
        expected: SnapshotGeneration,
        actual: SnapshotGeneration,
    },
}

/// One explicitly requested attempt to rebuild missing CPU pixels.
///
/// A demand remains armed while true-Iconic admission is pending, but the
/// `(demand_epoch, capacity_epoch)` pair may be consumed only once.  A later
/// explicit request advances the first epoch; releasing cache capacity
/// advances the second.  Ordinary compositor frames change neither and
/// therefore cannot turn a readback/allocation failure into a frame-rate retry
/// loop.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct SnapshotRecaptureGate {
    demand_epoch: u64,
    last_attempt: Option<(u64, u64)>,
}

impl SnapshotRecaptureGate {
    /// Arm a fresh explicit admission/ensure demand.
    pub(crate) fn request(&mut self) {
        self.demand_epoch = self.demand_epoch.wrapping_add(1);
        if self.demand_epoch == 0 {
            self.demand_epoch = 1;
        }
    }

    /// Whether a retained source may be sampled for the current epochs.
    pub(crate) const fn is_due(&self, capacity_epoch: u64) -> bool {
        self.demand_epoch != 0
            && !matches!(
                self.last_attempt,
                Some((demand, capacity))
                    if demand == self.demand_epoch && capacity == capacity_epoch
            )
    }

    /// Consume the current epoch pair before issuing a fallible readback.
    pub(crate) fn begin_attempt(&mut self, capacity_epoch: u64) -> bool {
        if !self.is_due(capacity_epoch) {
            return false;
        }
        self.last_attempt = Some((self.demand_epoch, capacity_epoch));
        true
    }
}

/// Byte- and entry-bounded LRU. Only `RecapturableMapped` entries are victims.
#[derive(Debug)]
pub(crate) struct MinimizedSnapshotCache<K> {
    entries: HashMap<K, CacheEntry>,
    used_bytes: usize,
    use_clock: u64,
    max_bytes: usize,
    max_entries: usize,
    /// Advances only when a previously failed admission could become feasible
    /// without a new explicit request (for example after an Iconic pin is
    /// released or an entry is removed).
    capacity_epoch: u64,
}

impl<K> Default for MinimizedSnapshotCache<K>
where
    K: Clone + Eq + Hash,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<K> MinimizedSnapshotCache<K>
where
    K: Clone + Eq + Hash,
{
    pub(crate) fn new() -> Self {
        Self::with_limits(SNAPSHOT_CACHE_MAX_BYTES, SNAPSHOT_CACHE_MAX_ENTRIES)
    }

    pub(crate) fn with_limits(max_bytes: usize, max_entries: usize) -> Self {
        Self {
            entries: HashMap::new(),
            used_bytes: 0,
            use_clock: 0,
            max_bytes,
            max_entries,
            capacity_epoch: 1,
        }
    }

    fn advance_capacity_epoch(&mut self) {
        self.capacity_epoch = self.capacity_epoch.wrapping_add(1);
        if self.capacity_epoch == 0 {
            self.capacity_epoch = 1;
        }
    }

    fn next_use(&mut self) -> u64 {
        self.use_clock = self.use_clock.saturating_add(1);
        self.use_clock
    }

    pub(crate) fn admit(
        &mut self,
        key: K,
        snapshot: MinimizedSnapshot,
        retention: SnapshotRetention,
    ) -> AdmissionOutcome<K> {
        let previous_used_bytes = self.used_bytes;
        let previous_len = self.entries.len();
        let replaced = self.entries.get(&key);
        if let Some(entry) = replaced {
            if entry.snapshot.generation() == snapshot.generation() {
                return AdmissionOutcome::AlreadyCurrent;
            }
            if entry.snapshot.generation() > snapshot.generation() {
                return AdmissionOutcome::RejectedStale;
            }
            // A newer capture must not silently revoke the guarantee already
            // handed to an in-flight/committed Iconic transition. Releasing
            // the exact reservation is the only operation that can make this
            // key replaceable again.
            if entry.retention == SnapshotRetention::IconicPinned {
                return AdmissionOutcome::RejectedCapacity;
            }
        }

        let replaced_bytes = replaced.map_or(0, |entry| entry.snapshot.byte_len());
        let mut planned_bytes = self.used_bytes - replaced_bytes + snapshot.byte_len();
        let mut planned_entries = self.entries.len() - usize::from(replaced.is_some()) + 1;
        if snapshot.byte_len() > self.max_bytes || self.max_entries == 0 {
            return AdmissionOutcome::RejectedCapacity;
        }

        let mut candidates: Vec<_> = self
            .entries
            .iter()
            .filter(|(candidate_key, entry)| {
                *candidate_key != &key && entry.retention == SnapshotRetention::RecapturableMapped
            })
            .map(|(candidate_key, entry)| {
                (
                    candidate_key.clone(),
                    entry.last_use,
                    entry.snapshot.byte_len(),
                )
            })
            .collect();
        candidates.sort_unstable_by_key(|(_, last_use, _)| *last_use);

        let mut evicted = Vec::new();
        for (candidate_key, _, candidate_bytes) in candidates {
            if planned_bytes <= self.max_bytes && planned_entries <= self.max_entries {
                break;
            }
            planned_bytes -= candidate_bytes;
            planned_entries -= 1;
            evicted.push(candidate_key);
        }
        if planned_bytes > self.max_bytes || planned_entries > self.max_entries {
            return AdmissionOutcome::RejectedCapacity;
        }

        for victim in &evicted {
            let removed = self
                .entries
                .remove(victim)
                .expect("planned snapshot LRU victim must still exist");
            self.used_bytes -= removed.snapshot.byte_len();
        }
        if let Some(old) = self.entries.remove(&key) {
            self.used_bytes -= old.snapshot.byte_len();
        }

        let last_use = self.next_use();
        self.used_bytes += snapshot.byte_len();
        self.entries.insert(
            key,
            CacheEntry {
                snapshot,
                retention,
                last_use,
            },
        );
        if self.used_bytes < previous_used_bytes || self.entries.len() < previous_len {
            self.advance_capacity_epoch();
        }
        AdmissionOutcome::Admitted { evicted }
    }

    pub(crate) fn get(&mut self, key: &K) -> Option<&MinimizedSnapshot> {
        let last_use = self.next_use();
        let entry = self.entries.get_mut(key)?;
        entry.last_use = last_use;
        Some(&entry.snapshot)
    }

    pub(crate) fn peek(&self, key: &K) -> Option<&MinimizedSnapshot> {
        self.entries.get(key).map(|entry| &entry.snapshot)
    }

    /// Atomically pin the CPU pixels for exactly the compositor's current
    /// capture epoch. Idempotent reservation of the same epoch is successful.
    pub(crate) fn reserve_iconic_snapshot(
        &mut self,
        key: &K,
        expected_generation: SnapshotGeneration,
    ) -> Result<SnapshotGeneration, IconicSnapshotReservationError> {
        let Some(entry) = self.entries.get_mut(key) else {
            return Err(IconicSnapshotReservationError::NoSnapshot);
        };
        let actual = entry.snapshot.generation();
        if actual != expected_generation {
            return Err(IconicSnapshotReservationError::GenerationMismatch {
                expected: expected_generation,
                actual,
            });
        }
        entry.retention = SnapshotRetention::IconicPinned;
        Ok(actual)
    }

    /// Release only the reservation named by `generation`; pixels and GPU
    /// mirrors remain available for the ordinary recapturable cache lifecycle.
    pub(crate) fn release_iconic_snapshot_reservation(
        &mut self,
        key: &K,
        generation: SnapshotGeneration,
    ) -> bool {
        let Some(entry) = self.entries.get_mut(key) else {
            return false;
        };
        if entry.snapshot.generation() != generation
            || entry.retention != SnapshotRetention::IconicPinned
        {
            return false;
        }
        entry.retention = SnapshotRetention::RecapturableMapped;
        self.advance_capacity_epoch();
        true
    }

    pub(crate) fn has_iconic_snapshot_reservation(
        &self,
        key: &K,
        generation: SnapshotGeneration,
    ) -> bool {
        self.entries.get(key).is_some_and(|entry| {
            entry.snapshot.generation() == generation
                && entry.retention == SnapshotRetention::IconicPinned
        })
    }

    pub(crate) fn remove(&mut self, key: &K) -> Option<MinimizedSnapshot> {
        let removed = self.entries.remove(key)?;
        self.used_bytes -= removed.snapshot.byte_len();
        self.advance_capacity_epoch();
        Some(removed.snapshot)
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub(crate) fn used_bytes(&self) -> usize {
        self.used_bytes
    }

    /// Epoch observed by `SnapshotRecaptureGate`. It deliberately does not
    /// advance on failed admissions or reads/touches.
    pub(crate) const fn capacity_epoch(&self) -> u64 {
        self.capacity_epoch
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(width: u32, height: u32, generation: u64, fill: u8) -> MinimizedSnapshot {
        MinimizedSnapshot::try_new(
            width,
            height,
            generation,
            true,
            vec![fill; width as usize * height as usize * SNAPSHOT_CHANNELS],
        )
        .unwrap()
    }

    #[test]
    fn dimensions_bound_both_axes_without_upscaling() {
        assert_eq!(snapshot_dimensions(0, 10), None);
        assert_eq!(snapshot_dimensions(10, 0), None);
        assert_eq!(snapshot_dimensions(100, 50), Some((100, 50)));
        assert_eq!(snapshot_dimensions(512, 200), Some((256, 100)));
        assert_eq!(snapshot_dimensions(200, 400), Some((96, 192)));
        assert_eq!(snapshot_dimensions(1, u32::MAX), Some((1, 192)));
        assert_eq!(snapshot_dimensions(u32::MAX, 1), Some((256, 1)));
    }

    #[test]
    fn full_envelope_matches_the_global_byte_budget_exactly() {
        let one = SNAPSHOT_MAX_WIDTH as usize * SNAPSHOT_MAX_HEIGHT as usize * SNAPSHOT_CHANNELS;
        assert_eq!(one * SNAPSHOT_CACHE_MAX_ENTRIES, SNAPSHOT_CACHE_MAX_BYTES);
    }

    #[test]
    fn staged_downsample_shader_has_a_full_four_by_four_footprint() {
        // One occurrence is the helper declaration; the remaining sixteen
        // are taps in main(). Keep this structural test until headless linking
        // moves into the backend wiring phase.
        assert_eq!(
            THUMBNAIL_DOWNSAMPLE_FRAGMENT_SHADER
                .matches("sample_clamped(")
                .count()
                - 1,
            THUMBNAIL_DOWNSAMPLE_TAPS
        );
        assert!(THUMBNAIL_DOWNSAMPLE_FRAGMENT_SHADER.contains("u_output_size"));
        assert!(THUMBNAIL_DOWNSAMPLE_FRAGMENT_SHADER.contains("u_has_alpha"));
    }

    #[test]
    fn shader_opacity_preserves_only_declared_source_alpha() {
        assert_eq!(snapshot_shader_opacity(true), -1.0);
        assert_eq!(snapshot_shader_opacity(false), 1.0);
    }

    #[test]
    fn snapshot_validation_is_strict() {
        assert_eq!(
            MinimizedSnapshot::try_new(0, 1, 1, true, Vec::<u8>::new()).unwrap_err(),
            SnapshotError::ZeroDimension
        );
        assert_eq!(
            MinimizedSnapshot::try_new(SNAPSHOT_MAX_WIDTH + 1, 1, 1, true, vec![0; 4]).unwrap_err(),
            SnapshotError::WidthExceedsLimit
        );
        assert_eq!(
            MinimizedSnapshot::try_new(1, SNAPSHOT_MAX_HEIGHT + 1, 1, true, vec![0; 4])
                .unwrap_err(),
            SnapshotError::HeightExceedsLimit
        );
        assert_eq!(
            MinimizedSnapshot::try_new(1, 1, 0, true, vec![0; 4]).unwrap_err(),
            SnapshotError::InvalidGeneration
        );
        assert_eq!(
            MinimizedSnapshot::try_new(2, 1, 1, true, vec![0; 7]).unwrap_err(),
            SnapshotError::ByteLengthMismatch {
                expected: 8,
                actual: 7,
            }
        );
        assert_eq!(
            MinimizedSnapshot::try_new(2, 1, 1, true, vec![0; 9]).unwrap_err(),
            SnapshotError::ByteLengthMismatch {
                expected: 8,
                actual: 9,
            }
        );
    }

    #[test]
    fn snapshot_keeps_top_left_rows_and_shares_the_arc() {
        let bytes: Arc<[u8]> = Arc::from([1, 2, 3, 4, 5, 6, 7, 8]);
        let saved = bytes.clone();
        let image = MinimizedSnapshot::try_new(1, 2, 7, true, bytes).unwrap();
        assert!(Arc::ptr_eq(image.rgba(), &saved));
        assert_eq!(&**image.rgba(), &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(image.width(), 1);
        assert_eq!(image.height(), 2);
        assert_eq!(image.generation().get(), 7);
        assert!(image.has_alpha());
        let opaque = MinimizedSnapshot::try_new(1, 1, 8, false, vec![9; 4]).unwrap();
        assert!(!opaque.has_alpha());
    }

    #[test]
    fn source_policy_never_uses_low_resolution_for_restore() {
        let all = [
            ThumbnailSource::Placeholder,
            ThumbnailSource::CpuSnapshot,
            ThumbnailSource::GpuSnapshot,
            ThumbnailSource::LiveMappedTexture,
            ThumbnailSource::RetainedVisual,
        ];
        assert_eq!(
            preferred_thumbnail_source(ThumbnailPurpose::StaticDockCard, all),
            Some(ThumbnailSource::GpuSnapshot)
        );
        assert_eq!(
            preferred_thumbnail_source(ThumbnailPurpose::HoverPreview, all),
            Some(ThumbnailSource::RetainedVisual)
        );
        assert_eq!(
            preferred_thumbnail_source(
                ThumbnailPurpose::StaticDockCard,
                [ThumbnailSource::Placeholder, ThumbnailSource::CpuSnapshot]
            ),
            Some(ThumbnailSource::CpuSnapshot)
        );
        assert_eq!(
            preferred_thumbnail_source(
                ThumbnailPurpose::RestoreAnimation,
                [ThumbnailSource::CpuSnapshot, ThumbnailSource::Placeholder]
            ),
            None
        );
        assert_eq!(
            preferred_thumbnail_source(ThumbnailPurpose::RestoreAnimation, all.into_iter().rev()),
            Some(ThumbnailSource::RetainedVisual)
        );
    }

    #[test]
    fn lru_touch_protects_the_recent_recapturable_entry() {
        let mut cache = MinimizedSnapshotCache::with_limits(8, 2);
        assert!(matches!(
            cache.admit(1, snapshot(1, 1, 1, 1), SnapshotRetention::RecapturableMapped),
            AdmissionOutcome::Admitted { evicted } if evicted.is_empty()
        ));
        cache.admit(
            2,
            snapshot(1, 1, 1, 2),
            SnapshotRetention::RecapturableMapped,
        );
        assert!(cache.get(&1).is_some());
        assert_eq!(
            cache.admit(
                3,
                snapshot(1, 1, 1, 3),
                SnapshotRetention::RecapturableMapped
            ),
            AdmissionOutcome::Admitted { evicted: vec![2] }
        );
        assert!(cache.peek(&1).is_some());
        assert!(cache.peek(&2).is_none());
        assert!(cache.peek(&3).is_some());
        assert_eq!(cache.used_bytes(), 8);
    }

    #[test]
    fn iconic_entries_are_never_lru_victims() {
        let mut cache = MinimizedSnapshotCache::with_limits(8, 2);
        cache.admit(
            1,
            snapshot(1, 1, 1, 1),
            SnapshotRetention::RecapturableMapped,
        );
        let generation = SnapshotGeneration::new(1).unwrap();
        assert_eq!(
            cache.reserve_iconic_snapshot(&1, generation),
            Ok(generation)
        );
        cache.admit(
            2,
            snapshot(1, 1, 1, 2),
            SnapshotRetention::RecapturableMapped,
        );
        assert_eq!(
            cache.admit(
                3,
                snapshot(1, 1, 1, 3),
                SnapshotRetention::RecapturableMapped
            ),
            AdmissionOutcome::Admitted { evicted: vec![2] }
        );
        assert!(cache.peek(&1).is_some());
        assert!(cache.peek(&2).is_none());
    }

    #[test]
    fn pinned_capacity_rejection_is_atomic() {
        let mut cache = MinimizedSnapshotCache::with_limits(8, 2);
        for key in [1, 2] {
            cache.admit(
                key,
                snapshot(1, 1, 1, key as u8),
                SnapshotRetention::RecapturableMapped,
            );
            let generation = SnapshotGeneration::new(1).unwrap();
            assert_eq!(
                cache.reserve_iconic_snapshot(&key, generation),
                Ok(generation)
            );
        }
        let before_bytes = cache.used_bytes();
        assert_eq!(
            cache.admit(
                3,
                snapshot(1, 1, 1, 3),
                SnapshotRetention::RecapturableMapped
            ),
            AdmissionOutcome::RejectedCapacity
        );
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.used_bytes(), before_bytes);
        assert!(cache.peek(&1).is_some());
        assert!(cache.peek(&2).is_some());
        assert!(cache.peek(&3).is_none());
    }

    #[test]
    fn generation_rejects_stale_replacement_and_newer_replacement_is_exact() {
        let mut cache = MinimizedSnapshotCache::with_limits(8, 2);
        cache.admit(
            1,
            snapshot(1, 1, 4, 4),
            SnapshotRetention::RecapturableMapped,
        );
        assert_eq!(
            cache.admit(1, snapshot(1, 1, 4, 9), SnapshotRetention::IconicPinned),
            AdmissionOutcome::AlreadyCurrent
        );
        assert_eq!(cache.peek(&1).unwrap().rgba()[0], 4);
        assert_eq!(
            cache.admit(1, snapshot(1, 1, 3, 3), SnapshotRetention::IconicPinned),
            AdmissionOutcome::RejectedStale
        );
        assert_eq!(cache.peek(&1).unwrap().generation().get(), 4);
        assert_eq!(
            cache.admit(1, snapshot(2, 1, 5, 5), SnapshotRetention::IconicPinned),
            AdmissionOutcome::Admitted { evicted: vec![] }
        );
        assert_eq!(cache.peek(&1).unwrap().generation().get(), 5);
        assert_eq!(cache.used_bytes(), 8);
    }

    #[test]
    fn failed_replacement_does_not_discard_the_previous_value() {
        let mut cache = MinimizedSnapshotCache::with_limits(4, 1);
        cache.admit(
            1,
            snapshot(1, 1, 1, 1),
            SnapshotRetention::RecapturableMapped,
        );
        assert_eq!(
            cache.admit(1, snapshot(2, 1, 2, 2), SnapshotRetention::IconicPinned),
            AdmissionOutcome::RejectedCapacity
        );
        assert_eq!(cache.peek(&1).unwrap().generation().get(), 1);
        assert_eq!(cache.used_bytes(), 4);
    }

    #[test]
    fn iconic_reservation_is_generation_exact_and_release_keeps_pixels() {
        let mut cache = MinimizedSnapshotCache::with_limits(4, 1);
        cache.admit(
            9,
            snapshot(1, 1, 7, 9),
            SnapshotRetention::RecapturableMapped,
        );

        let generation = SnapshotGeneration::new(7).unwrap();
        let stale = SnapshotGeneration::new(6).unwrap();
        assert_eq!(
            cache.reserve_iconic_snapshot(&10, generation),
            Err(IconicSnapshotReservationError::NoSnapshot)
        );
        assert_eq!(
            cache.reserve_iconic_snapshot(&9, stale),
            Err(IconicSnapshotReservationError::GenerationMismatch {
                expected: stale,
                actual: generation,
            })
        );
        assert!(!cache.has_iconic_snapshot_reservation(&9, generation));
        assert_eq!(
            cache.reserve_iconic_snapshot(&9, generation),
            Ok(generation)
        );
        assert!(cache.has_iconic_snapshot_reservation(&9, generation));
        assert!(!cache.has_iconic_snapshot_reservation(&9, stale));

        // A stale cancellation cannot unpin the current reservation.
        assert!(!cache.release_iconic_snapshot_reservation(&9, stale));
        assert!(cache.has_iconic_snapshot_reservation(&9, generation));
        assert!(cache.release_iconic_snapshot_reservation(&9, generation));
        assert!(!cache.has_iconic_snapshot_reservation(&9, generation));
        assert!(cache.peek(&9).is_some(), "release must retain the CPU copy");
        assert!(!cache.release_iconic_snapshot_reservation(&9, generation));

        assert_eq!(cache.remove(&9).unwrap().byte_len(), 4);
        assert!(cache.is_empty());
        assert_eq!(cache.used_bytes(), 0);
        assert!(cache.remove(&9).is_none());
    }

    #[test]
    fn newer_capture_cannot_replace_an_older_iconic_reservation() {
        let mut cache = MinimizedSnapshotCache::with_limits(4, 1);
        cache.admit(
            42,
            snapshot(1, 1, 7, 7),
            SnapshotRetention::RecapturableMapped,
        );
        let pinned = SnapshotGeneration::new(7).unwrap();
        assert_eq!(cache.reserve_iconic_snapshot(&42, pinned), Ok(pinned));

        assert_eq!(
            cache.admit(
                42,
                snapshot(1, 1, 8, 8),
                SnapshotRetention::RecapturableMapped,
            ),
            AdmissionOutcome::RejectedCapacity
        );
        let retained = cache.peek(&42).unwrap();
        assert_eq!(retained.generation(), pinned);
        assert_eq!(retained.rgba()[0], 7);
        assert!(cache.has_iconic_snapshot_reservation(&42, pinned));

        assert!(cache.release_iconic_snapshot_reservation(&42, pinned));
        assert_eq!(
            cache.admit(
                42,
                snapshot(1, 1, 8, 8),
                SnapshotRetention::RecapturableMapped,
            ),
            AdmissionOutcome::Admitted { evicted: vec![] }
        );
        assert_eq!(cache.peek(&42).unwrap().generation().get(), 8);
    }

    #[test]
    fn recapture_gate_retries_once_per_demand_or_capacity_epoch() {
        let mut gate = SnapshotRecaptureGate::default();
        let mut cache = MinimizedSnapshotCache::<u32>::with_limits(4, 1);

        assert!(!gate.is_due(cache.capacity_epoch()));
        gate.request();
        assert!(gate.begin_attempt(cache.capacity_epoch()));
        for _ in 0..32 {
            assert!(
                !gate.begin_attempt(cache.capacity_epoch()),
                "ordinary frames must not re-consume one readback demand"
            );
        }

        // Failed admission changes no cache state and therefore cannot unlock
        // another attempt by itself.
        let epoch = cache.capacity_epoch();
        assert_eq!(
            cache.admit(
                1,
                snapshot(2, 1, 1, 1),
                SnapshotRetention::RecapturableMapped,
            ),
            AdmissionOutcome::RejectedCapacity
        );
        assert_eq!(cache.capacity_epoch(), epoch);
        assert!(!gate.begin_attempt(cache.capacity_epoch()));

        gate.request();
        assert!(gate.begin_attempt(cache.capacity_epoch()));
        assert!(!gate.begin_attempt(cache.capacity_epoch()));
    }

    #[test]
    fn pinned_capacity_release_unlocks_retained_one_shot_and_single_admission() {
        const A: u32 = 1;
        const B: u32 = 2;
        let generation = SnapshotGeneration::new(1).unwrap();
        let mut cache = MinimizedSnapshotCache::with_limits(4, 1);
        assert!(matches!(
            cache.admit(
                A,
                snapshot(1, 1, 1, 1),
                SnapshotRetention::RecapturableMapped,
            ),
            AdmissionOutcome::Admitted { evicted } if evicted.is_empty()
        ));
        assert_eq!(
            cache.reserve_iconic_snapshot(&A, generation),
            Ok(generation)
        );

        // B owns a full retained visual outside this CPU cache. Its explicit
        // Iconic demand is nevertheless allowed exactly one bounded readback.
        let retained_visual = true;
        let mut gate = SnapshotRecaptureGate::default();
        gate.request();
        let mut readbacks = 0;
        assert!(retained_visual && gate.begin_attempt(cache.capacity_epoch()));
        readbacks += 1;
        assert_eq!(
            cache.admit(
                B,
                snapshot(1, 1, 1, 2),
                SnapshotRetention::RecapturableMapped,
            ),
            AdmissionOutcome::RejectedCapacity
        );
        assert_eq!(
            cache.reserve_iconic_snapshot(&B, generation),
            Err(IconicSnapshotReservationError::NoSnapshot)
        );
        for _ in 0..32 {
            assert!(!gate.begin_attempt(cache.capacity_epoch()));
        }
        assert_eq!(readbacks, 1);

        // Releasing A changes only the capacity epoch. The still-armed demand
        // may now sample B's retained pixels once, evict A, pin B, and send one
        // checked unmap. Subsequent admission service is no longer awaiting.
        assert!(cache.release_iconic_snapshot_reservation(&A, generation));
        assert!(
            gate.is_due(cache.capacity_epoch()),
            "released capacity must make the retained demand retryable"
        );
        gate.request();
        assert!(gate.begin_attempt(cache.capacity_epoch()));
        readbacks += 1;
        assert_eq!(
            cache.admit(
                B,
                snapshot(1, 1, 1, 2),
                SnapshotRetention::RecapturableMapped,
            ),
            AdmissionOutcome::Admitted { evicted: vec![A] }
        );
        assert_eq!(
            cache.reserve_iconic_snapshot(&B, generation),
            Ok(generation)
        );

        let mut awaiting_admission = true;
        let mut checked_unmaps = 0;
        if awaiting_admission && cache.has_iconic_snapshot_reservation(&B, generation) {
            awaiting_admission = false;
            checked_unmaps += 1;
        }
        gate = SnapshotRecaptureGate::default();
        for _ in 0..32 {
            assert!(!gate.begin_attempt(cache.capacity_epoch()));
            if awaiting_admission && cache.has_iconic_snapshot_reservation(&B, generation) {
                checked_unmaps += 1;
            }
        }
        assert_eq!(readbacks, 2);
        assert_eq!(checked_unmaps, 1);
    }
}
