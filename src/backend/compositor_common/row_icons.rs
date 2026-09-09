//! Per-row application icons for the launcher and the window switcher.
//!
//! The system-UI payload (`SystemUiOverlay`) is strings-only and its
//! construction site is shared with features that must not see this channel,
//! so row icons travel in a side band instead: the window manager publishes
//! the raster paths it resolved for the rows it is about to push, and the
//! compositor picks the band up when — and only when — the overlay it
//! receives carries exactly those rows. Everything here is the shared half of
//! that exchange: the band itself, the decode workers, and the texture-cache
//! policy. Uploading pixels and deleting names stay with each renderer, the
//! same split `wallpaper.rs` uses for the picker's side preview.
//!
//! Three rules the pieces encode:
//!
//! *The event thread and the frame never touch a file.* The WM resolves icon
//! names to raster paths at row-build time through `xbar_core`'s cached
//! resolver; the decode below runs on a gated worker and reports over a
//! channel, exactly like the wallpaper loaders.
//!
//! *A miss is an answer.* A path that will not decode is remembered for the
//! life of the panel, so a broken theme cannot turn a 500-row scroll into a
//! retry storm of failed decodes.
//!
//! *Text-only panels are untouched.* The band is `None` for them, the
//! geometry module reserves no icon column, and the pixels are identical to a
//! build without this feature.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Condvar, Mutex, OnceLock, mpsc};

use crate::sync_ext::MutexExt as _;

/// Long-edge decode bound for a row icon: twice the drawn slot
/// ([`super::system_ui_panel::ROW_ICON_PX`]), so the texture lands sharp when
/// the source is larger and is never upscaled at decode time when it is not.
pub(crate) const ROW_ICON_DECODE_EDGE: u32 = 48;

/// Textures held at once. The visible window is at most 14 rows today; the
/// headroom lets a list scrolled back and forth keep its icons without
/// re-decoding, while a long one-way scroll cannot grow the cache without
/// limit.
const MAX_TEXTURES: usize = 64;

/// Decodes in flight at once. Beyond this a new row simply waits for a later
/// sync to ask again — by then a slot has usually drained.
const MAX_PENDING: usize = 32;

/// Misses remembered per panel. The launcher list can be long; this keeps the
/// no-retry promise without letting a theme full of holes grow the set
/// forever. A path past the cap may be retried once when it is scrolled back
/// into view — rare, and bounded by the visible rows per sync.
const MAX_MISSES: usize = 256;

/// Rows the band ever carries. The windowed lists send at most 28 today; the
/// bound keeps a pathological caller from turning the band into an
/// unbounded allocation.
const MAX_BAND_ROWS: usize = 64;

/// Decoded pixels of one row icon, ready for a renderer-specific upload.
pub(crate) struct RowIconData {
    pub(crate) rgba: Vec<u8>,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

fn decode_gate() -> &'static (Mutex<usize>, Condvar) {
    static GATE: OnceLock<(Mutex<usize>, Condvar)> = OnceLock::new();
    GATE.get_or_init(|| {
        let max = std::thread::available_parallelism()
            .map(|n| n.get().min(4))
            .unwrap_or(2);
        (Mutex::new(max), Condvar::new())
    })
}

/// RAII permit for the row-icon decode gate, the wallpaper loaders' pattern:
/// blocks until a permit is free, returns it on drop.
struct DecodePermit;

impl DecodePermit {
    fn acquire() -> Self {
        let (lock, cvar) = decode_gate();
        let mut avail = lock.lock_safe();
        while *avail == 0 {
            avail = cvar.wait(avail).unwrap_or_else(|e| e.into_inner());
        }
        *avail -= 1;
        DecodePermit
    }
}

impl Drop for DecodePermit {
    fn drop(&mut self) {
        let (lock, cvar) = decode_gate();
        let mut avail = lock.lock_safe();
        *avail += 1;
        cvar.notify_one();
    }
}

/// Decode one icon on a background thread. Same worker pattern as the
/// wallpaper picker's side preview — decode gate, bounded thumbnail, channel
/// back — and the same quiet failure: an unreadable file sends nothing, which
/// the cache records as a miss instead of retrying.
pub(crate) fn decode_async(path: &str) -> mpsc::Receiver<RowIconData> {
    let (tx, rx) = mpsc::channel();
    let path = path.to_string();
    std::thread::spawn(move || {
        // Bound concurrent decodes; released when this thread exits.
        let _permit = DecodePermit::acquire();
        let img = match image::open(&path) {
            Ok(img) => img,
            Err(e) => {
                log::debug!("compositor: no row icon for '{}': {}", path, e);
                return;
            }
        };
        let img = if img.width() > ROW_ICON_DECODE_EDGE || img.height() > ROW_ICON_DECODE_EDGE {
            img.resize(
                ROW_ICON_DECODE_EDGE,
                ROW_ICON_DECODE_EDGE,
                image::imageops::FilterType::Lanczos3,
            )
        } else {
            img
        };
        let rgba = img.to_rgba8();
        let (w, h) = (rgba.width(), rgba.height());
        let _ = tx.send(RowIconData {
            rgba: rgba.into_raw(),
            width: w,
            height: h,
        });
    });
    rx
}

/// The band one panel's row icons travel in: the exact rows they belong to,
/// plus the resolved raster paths aligned with them.
struct RowIconBand {
    items: Vec<String>,
    icons: Arc<[Option<String>]>,
}

static ROW_ICON_BAND: Mutex<Option<RowIconBand>> = Mutex::new(None);

/// Publish the row icons of the panel content being pushed this sync.
///
/// Called from the same thread and call stack that hands the overlay to the
/// backend, immediately before it — so the band always describes the payload
/// in flight, never a stale one. `None` (any panel without row icons) clears
/// it. A malformed call — a band not aligned with its rows, one with no icon
/// at all, or an oversized one — publishes nothing, which is the text-only
/// layout: the band fails safe.
pub(crate) fn publish(items: &[String], icons: Option<&[Option<String>]>) {
    let band = icons.and_then(|icons| {
        (icons.len() == items.len()
            && items.len() <= MAX_BAND_ROWS
            && icons.iter().any(Option::is_some))
        .then(|| RowIconBand {
            items: items.to_vec(),
            icons: Arc::from(icons),
        })
    });
    *ROW_ICON_BAND.lock_safe() = band;
}

/// The band's icons, when they provably belong to `items` — the overlay the
/// compositor is receiving. The content match is what lets a synthesized row
/// set (the launcher's direct-command view replaces its rows after the parts
/// were built) fall back to the text-only layout instead of drawing icons
/// next to rows they were never resolved for.
pub(crate) fn icons_for(items: &[String]) -> Option<Arc<[Option<String>]>> {
    let band = ROW_ICON_BAND.lock_safe();
    let band = band.as_ref()?;
    (band.items == items && band.icons.len() == items.len()).then(|| band.icons.clone())
}

/// The placeholder every launcher/switcher window row starts with:
/// FontAwesome 4.7's fa-window-maximize and the two-space gap after it. The
/// window manager always emits it — the band's content match keys on the
/// rows' exact bytes, so they never change — and the compositor strips it
/// from the rasterized text of a row whose real icon is being drawn
/// ([`items_text`]), so the glyph and the icon never show side by side.
pub(crate) const WINDOW_ROW_GLYPH: &str = "\u{f2d0}  ";

/// The items block as it should be rasterized: the plain newline join, except
/// that a row whose real icon is on the GPU this frame — its resolved path
/// has an uploaded texture, per `uploaded` — loses its [`WINDOW_ROW_GLYPH`]
/// prefix, which the drawn icon replaces. A row whose icon is still decoding,
/// remembered as a miss, or never resolved keeps the glyph: the fail-safe
/// placeholder. Without a band this is the plain join, byte for byte, and a
/// row that never carried the prefix always passes through untouched.
pub(crate) fn items_text(
    items: &[String],
    icons: Option<&[Option<String>]>,
    uploaded: impl Fn(&str) -> bool,
) -> String {
    let Some(icons) = icons else {
        return items.join("\n");
    };
    items
        .iter()
        .enumerate()
        .map(
            |(row, text)| match icons.get(row).and_then(Option::as_deref) {
                Some(path) if uploaded(path) => text.strip_prefix(WINDOW_ROW_GLYPH).unwrap_or(text),
                _ => text.as_str(),
            },
        )
        .collect::<Vec<_>>()
        .join("\n")
}

/// Decoded row-icon textures plus the in-flight decodes and the remembered
/// misses, keyed by resolved path. `T` is the renderer's texture handle; the
/// policy here never touches GL — uploads and deletes are the compositor's,
/// which is also what makes every transition testable without a context.
pub(crate) struct RowIconCache<T> {
    textures: HashMap<String, T>,
    /// Least-recently-used path first. An explicit order keeps eviction off
    /// `HashMap`'s randomized iteration.
    recency: VecDeque<String>,
    /// In-flight decodes. A dropped receiver turns a superseded worker's send
    /// into a no-op, so clearing or capping never waits on a thread.
    pending: HashMap<String, mpsc::Receiver<RowIconData>>,
    /// Paths whose decode already failed. Remembered for the life of the
    /// panel: a miss must not be re-decoded on every sync.
    missed: HashSet<String>,
}

impl<T> Default for RowIconCache<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> RowIconCache<T> {
    pub(crate) fn new() -> Self {
        Self {
            textures: HashMap::new(),
            recency: VecDeque::new(),
            pending: HashMap::new(),
            missed: HashSet::new(),
        }
    }

    /// The texture uploaded for `path`, if one landed.
    pub(crate) fn get(&self, path: &str) -> Option<&T> {
        self.textures.get(path)
    }

    /// Whether any decode is still in flight — the render loop's reason to
    /// keep polling frames coming, the side preview's wake-up pattern.
    pub(crate) fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Start decodes for the wanted paths the cache has no answer for. A
    /// cached texture or an in-flight decode is just marked recent; a
    /// remembered miss is left alone. Wanted rows are touched so a row on
    /// screen is never the eviction candidate.
    pub(crate) fn sync(&mut self, icons: &[Option<String>]) {
        for path in icons.iter().flatten() {
            let path = path.as_str();
            if self.textures.contains_key(path) {
                self.touch(path);
                continue;
            }
            if self.missed.contains(path) || self.pending.contains_key(path) {
                continue;
            }
            // A full pipe asks again on a later sync rather than queueing
            // unboundedly; decodes drain in milliseconds.
            if self.pending.len() >= MAX_PENDING {
                continue;
            }
            self.pending.insert(path.to_string(), decode_async(path));
        }
    }

    /// Collect the decodes that finished since the last poll. A worker that
    /// finished without sending — the unreadable or undecodable file — joins
    /// `missed` and is not retried while this panel lives.
    pub(crate) fn drain_completed(&mut self) -> Vec<(String, RowIconData)> {
        let mut completed = Vec::new();
        let mut failed = Vec::new();
        self.pending.retain(|path, rx| match rx.try_recv() {
            Ok(data) => {
                completed.push((path.clone(), data));
                false
            }
            Err(mpsc::TryRecvError::Empty) => true,
            Err(mpsc::TryRecvError::Disconnected) => {
                failed.push(path.clone());
                false
            }
        });
        for path in failed {
            if self.missed.len() < MAX_MISSES {
                self.missed.insert(path);
            }
        }
        completed
    }

    /// Remember that `path` never decoded. For the failures only the renderer
    /// can see: an upload that the GL side refused.
    pub(crate) fn mark_missed(&mut self, path: &str) {
        if self.missed.len() < MAX_MISSES {
            self.missed.insert(path.to_string());
        }
    }

    /// The path an insert of `new_path` would evict right now, mirroring
    /// [`insert_texture`](Self::insert_texture)'s policy: a path already
    /// cached is replaced in place (nothing is evicted), and past
    /// [`MAX_TEXTURES`] the least-recently-used entry goes. The compositor
    /// asks before inserting so that a visible row losing its icon can have
    /// its text re-rasterized with the generic glyph back.
    pub(crate) fn eviction_candidate(&self, new_path: &str) -> Option<&str> {
        if self.textures.contains_key(new_path) || self.textures.len() < MAX_TEXTURES {
            return None;
        }
        self.recency.front().map(String::as_str)
    }

    /// Install an uploaded texture, evicting the least-recently-used one past
    /// [`MAX_TEXTURES`]. The evicted (or defensively replaced) handle goes back
    /// to the caller, whose GL context deletes it.
    pub(crate) fn insert_texture(&mut self, path: String, texture: T) -> Option<T> {
        // Requests are deduplicated, so a decode lands on an empty slot; if
        // one ever did replace, the orphan is still handed back for deletion.
        if let Some(old) = self.textures.insert(path.clone(), texture) {
            self.touch(&path);
            return Some(old);
        }
        self.recency.push_back(path);
        if self.textures.len() > MAX_TEXTURES {
            let oldest = self
                .recency
                .pop_front()
                .expect("a full icon cache has an eviction candidate");
            return self.textures.remove(&oldest);
        }
        None
    }

    /// Empty the cache, returning every texture handle for the caller to
    /// delete (or retire until a GL context exists). Dropping the receivers
    /// makes in-flight workers' sends land nowhere; the misses go with the
    /// panel they were remembered for, so a next panel self-heals an icon
    /// that appeared in the meantime.
    pub(crate) fn clear(&mut self) -> Vec<T> {
        self.pending.clear();
        self.missed.clear();
        self.recency.clear();
        self.textures.drain().map(|(_, texture)| texture).collect()
    }

    fn touch(&mut self, path: &str) {
        let Some(position) = self.recency.iter().position(|cached| cached == path) else {
            return;
        };
        let key = self
            .recency
            .remove(position)
            .expect("the recorded icon cache position exists");
        self.recency.push_back(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data() -> RowIconData {
        RowIconData {
            rgba: vec![0; 4],
            width: 1,
            height: 1,
        }
    }

    fn strings(rows: &[&str]) -> Vec<String> {
        rows.iter().map(|row| row.to_string()).collect()
    }

    /// The band is one process-wide slot, so the tests that publish one must
    /// not run concurrently — a parallel clear would pull another test's
    /// fixture from under its assertion.
    static BAND_TESTS: Mutex<()> = Mutex::new(());

    #[test]
    fn a_published_band_is_handed_out_only_for_its_own_rows() {
        let _serial = BAND_TESTS.lock_safe();
        let items = strings(&["Firefox", "Terminal"]);
        let icons = vec![Some("/icons/firefox.png".to_string()), None];
        publish(&items, Some(&icons));

        let band = icons_for(&items).expect("the band matches the rows it came with");
        assert_eq!(band.as_ref(), icons.as_slice());
        // Same length, different content: no match.
        assert!(icons_for(&strings(&["Firefox", "Files"])).is_none());
        // A prefix is not the panel either.
        assert!(icons_for(&strings(&["Firefox"])).is_none());
    }

    #[test]
    fn panels_without_icons_clear_the_band() {
        let _serial = BAND_TESTS.lock_safe();
        let items = strings(&["Firefox"]);
        publish(&items, Some(&[Some("/icons/firefox.png".to_string())]));
        publish(&items, None);
        assert!(icons_for(&items).is_none());
    }

    #[test]
    fn malformed_bands_publish_nothing() {
        let _serial = BAND_TESTS.lock_safe();
        let items = strings(&["Firefox", "Terminal"]);
        // An icon vec that does not align with the rows.
        publish(&items, Some(&[Some("/icons/firefox.png".to_string())]));
        assert!(icons_for(&items).is_none());
        // All misses: nothing to draw, nothing to reserve.
        publish(&items, Some(&[None, None]));
        assert!(icons_for(&items).is_none());
        // Beyond the row bound.
        let long = strings(&["row"; MAX_BAND_ROWS + 1]);
        let long_icons = vec![Some("/icons/a.png".to_string()); MAX_BAND_ROWS + 1];
        publish(&long, Some(&long_icons));
        assert!(icons_for(&long).is_none());
    }

    #[test]
    fn a_sync_requests_only_what_it_has_no_answer_for() {
        let mut cache = RowIconCache::<u32>::new();
        let want = vec![Some("/icons/a.png".to_string()), None];
        cache.sync(&want);
        assert_eq!(cache.pending.len(), 1);
        // A repeated sync does not duplicate the in-flight decode.
        cache.sync(&want);
        assert_eq!(cache.pending.len(), 1);

        // Once uploaded, the texture short-circuits later syncs.
        cache.pending.clear();
        assert!(cache.insert_texture("/icons/a.png".into(), 7).is_none());
        cache.sync(&want);
        assert!(cache.pending.is_empty());
        assert_eq!(cache.get("/icons/a.png"), Some(&7));
    }

    #[test]
    fn a_finished_decode_lands_and_a_failed_one_is_remembered() {
        let mut cache = RowIconCache::<u32>::new();
        let (tx, rx) = mpsc::channel();
        tx.send(data()).unwrap();
        cache.pending.insert("/icons/ok.png".into(), rx);
        let (tx, rx) = mpsc::channel::<RowIconData>();
        cache.pending.insert("/icons/gone.png".into(), rx);
        drop(tx); // the worker that never sends

        let completed = cache.drain_completed();
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].0, "/icons/ok.png");
        assert!(cache.pending.is_empty());
        assert!(cache.missed.contains("/icons/gone.png"));

        // The miss is an answer: wanting the row again starts no new decode.
        cache.sync(&[Some("/icons/gone.png".to_string())]);
        assert!(cache.pending.is_empty());
    }

    #[test]
    fn the_texture_cache_is_bounded_and_evicts_least_recent_first() {
        let mut cache = RowIconCache::<u32>::new();
        for index in 0..MAX_TEXTURES {
            assert!(
                cache
                    .insert_texture(format!("/icons/{index}.png"), index as u32)
                    .is_none()
            );
        }
        // Watching row 0 again makes it the hottest entry.
        cache.sync(&[Some("/icons/0.png".to_string())]);
        let evicted = cache.insert_texture("/icons/new.png".into(), 10_000);
        assert_eq!(
            evicted,
            Some(1),
            "row 1 was the coldest once row 0 was reused"
        );
        assert!(cache.get("/icons/1.png").is_none());
        assert_eq!(cache.get("/icons/0.png"), Some(&0));
        assert_eq!(cache.get("/icons/new.png"), Some(&10_000));
        assert_eq!(cache.textures.len(), MAX_TEXTURES);
    }

    #[test]
    fn a_pending_decode_over_cap_waits_for_a_later_sync() {
        let mut cache = RowIconCache::<u32>::new();
        let mut senders = Vec::new();
        for index in 0..MAX_PENDING {
            let (tx, rx) = mpsc::channel::<RowIconData>();
            senders.push(tx);
            cache.pending.insert(format!("/icons/p{index}.png"), rx);
        }
        cache.sync(&[Some("/icons/overflow.png".to_string())]);
        assert!(!cache.pending.contains_key("/icons/overflow.png"));
        // Once a slot drains, the same sync asks again.
        cache.pending.remove("/icons/p0.png");
        cache.sync(&[Some("/icons/overflow.png".to_string())]);
        assert!(cache.pending.contains_key("/icons/overflow.png"));
        // Keep the workers' sends deliverable so nothing here logs.
        drop(senders);
        cache.pending.clear();
    }

    #[test]
    fn clearing_hands_every_texture_back_and_forgets_the_rest() {
        let mut cache = RowIconCache::<u32>::new();
        cache.insert_texture("/icons/a.png".into(), 1);
        cache.insert_texture("/icons/b.png".into(), 2);
        let (_tx, rx) = mpsc::channel::<RowIconData>();
        cache.pending.insert("/icons/c.png".into(), rx);
        cache.mark_missed("/icons/d.png");

        let mut freed = cache.clear();
        freed.sort_unstable();
        assert_eq!(freed, vec![1, 2]);
        assert!(cache.textures.is_empty());
        assert!(!cache.has_pending());
        assert!(cache.missed.is_empty());
        assert!(cache.recency.is_empty());
    }

    #[test]
    fn an_uploaded_replacement_returns_the_orphan() {
        let mut cache = RowIconCache::<u32>::new();
        assert!(cache.insert_texture("/icons/a.png".into(), 1).is_none());
        assert_eq!(cache.insert_texture("/icons/a.png".into(), 2), Some(1));
        assert_eq!(cache.get("/icons/a.png"), Some(&2));
    }

    #[test]
    fn the_eviction_candidate_is_the_path_an_insert_would_evict() {
        let mut cache = RowIconCache::<u32>::new();
        for index in 0..MAX_TEXTURES {
            // Room left: an insert evicts nothing, and the candidate says so.
            assert!(cache.eviction_candidate("/icons/q.png").is_none());
            assert!(
                cache
                    .insert_texture(format!("/icons/{index}.png"), index as u32)
                    .is_none()
            );
        }
        // Full: the next new path evicts the coldest entry, named in advance.
        assert_eq!(
            cache.eviction_candidate("/icons/new.png"),
            Some("/icons/0.png")
        );
        assert_eq!(
            cache.insert_texture("/icons/new.png".into(), 10_000),
            Some(0)
        );
        // Replacing a cached path evicts nothing, and the candidate agrees.
        assert!(cache.eviction_candidate("/icons/new.png").is_none());
        assert_eq!(
            cache.insert_texture("/icons/new.png".into(), 10_001),
            Some(10_000)
        );
    }

    #[test]
    fn a_row_with_an_uploaded_icon_loses_the_generic_glyph() {
        let items = strings(&[
            &format!("{WINDOW_ROW_GLYPH}Firefox"),
            &format!("{WINDOW_ROW_GLYPH}Terminal"),
        ]);
        let icons = vec![
            Some("/icons/firefox.png".to_string()),
            Some("/icons/terminal.png".to_string()),
        ];
        // Only the first row's texture is uploaded: only its glyph is stripped.
        let text = items_text(&items, Some(&icons), |path| path == "/icons/firefox.png");
        assert_eq!(text, format!("Firefox\n{WINDOW_ROW_GLYPH}Terminal"));
    }

    #[test]
    fn rows_without_an_uploaded_icon_keep_the_glyph() {
        let items = strings(&[&format!("{WINDOW_ROW_GLYPH}Firefox")]);
        // Still decoding (a path, no texture yet) and never resolved (no path
        // at all) both keep the placeholder.
        for icons in [vec![Some("/icons/firefox.png".to_string())], vec![None]] {
            let text = items_text(&items, Some(&icons), |_| false);
            assert_eq!(text, items[0]);
        }
    }

    #[test]
    fn a_panel_without_an_icon_band_joins_byte_for_byte() {
        let items = strings(&[&format!("{WINDOW_ROW_GLYPH}Firefox"), "Plain row"]);
        // Even an always-true `uploaded` is never consulted without a band.
        assert_eq!(items_text(&items, None, |_| true), items.join("\n"));
    }

    #[test]
    fn rows_that_never_carried_the_glyph_pass_through_untouched() {
        // Application rows have no prefix to strip, uploaded icon or not.
        let items = strings(&["Firefox", "Terminal  \u{f120}"]);
        let icons = vec![
            Some("/icons/firefox.png".to_string()),
            Some("/icons/terminal.png".to_string()),
        ];
        assert_eq!(items_text(&items, Some(&icons), |_| true), items.join("\n"));
    }
}
