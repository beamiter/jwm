//! Wallpaper picker.
//!
//! JWM already renders wallpapers from `behavior.wallpaper`; this module only
//! finds the candidates and applies the choice through the same configuration
//! path a `set_config` would take, so there is one code path that changes the
//! wallpaper rather than two.
//!
//! Directory scanning and name formatting are pure so the extension filter,
//! the ordering, and the "which directory" decision are unit tested without
//! touching the user's pictures.

use std::path::{Path, PathBuf};

/// Picker-only identity. Never pass it to an image decoder or store it as a path.
pub(crate) const ASTRONOMY_CHOICE: &str = "jwm-effect:astronomy";
pub(crate) const SOLAR_SYSTEM_CHOICE: &str = "jwm-effect:solar-system";
pub(crate) fn is_effect_choice(choice: &str) -> bool {
    matches!(choice, ASTRONOMY_CHOICE | SOLAR_SYSTEM_CHOICE)
}

/// A fresh effect selection needs a resolved local texture (configured or
/// bundled). Re-selecting an active effect does not discard its loaded image.
pub(crate) fn astronomy_texture_ready(
    already_active: bool,
    texture: &str,
    texture_available: impl FnOnce(&str) -> bool,
) -> bool {
    already_active || texture_available(texture)
}

/// Highlight the manual daily choice even when the first output has an
/// explicit override. Otherwise reflect that output's currently resolved image.
pub(crate) fn picker_current_at(
    behavior: &crate::config::BehaviorConfig,
    active_tags: u32,
    day: i32,
) -> &str {
    use crate::backend::compositor_common::wallpaper::{
        daily_picker_wallpaper_at, resolve_wallpaper_for_tag_at,
    };
    if behavior.wallpaper_effect == "solar-system" {
        return SOLAR_SYSTEM_CHOICE;
    }
    if behavior.wallpaper_effect == "astronomy" {
        return ASTRONOMY_CHOICE;
    }
    daily_picker_wallpaper_at(behavior, day)
        .unwrap_or_else(|| resolve_wallpaper_for_tag_at(behavior, 0, active_tags, day).0)
}

/// Extensions the compositor's image loader handles.
const IMAGE_EXTENSIONS: [&str; 6] = ["png", "jpg", "jpeg", "webp", "bmp", "gif"];

/// Most entries listed; a pictures directory can be enormous and the panel
/// scrolls, but building and ordering rows for tens of thousands of pictures
/// would stall the frame that opened it.
///
/// Selection retains at most twice this many candidates while the worker
/// reads a bounded prefix of the directory. Only entries whose type is unresolved need a
/// stat, see [`select_wallpapers`].
pub const MAX_ENTRIES: usize = 200;

/// Stop pathological directories after a bounded number of raw entries,
/// including sidecars and unreadable entries. Ordering applies to this prefix.
pub const MAX_SCAN_ENTRIES: usize = 100_000;

/// Dropping the owner cancels its worker without joining on filesystem I/O.
#[derive(Debug, Default)]
pub struct ScanCancellation(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl ScanCancellation {
    pub fn observer(&self) -> impl Fn() -> bool + Send + 'static {
        let cancelled = std::sync::Arc::clone(&self.0);
        move || cancelled.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl Drop for ScanCancellation {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

#[derive(Debug)]
pub struct WallpaperListing {
    pub directory: PathBuf,
    pub paths: Vec<PathBuf>,
    /// Configuration identity at scan start and its original matching row key.
    /// Applied only if the configuration still has that identity at completion.
    pub selection: Option<(String, String)>,
    /// Keep partial and failed directory scans distinguishable from an empty
    /// folder. The event loop formats this without touching the filesystem.
    pub summary: ScanSummary,
}

impl From<(PathBuf, Vec<PathBuf>)> for WallpaperListing {
    fn from((directory, paths): (PathBuf, Vec<PathBuf>)) -> Self {
        let images = paths.len();
        Self {
            directory,
            paths,
            selection: None,
            summary: ScanSummary {
                images,
                ..Default::default()
            },
        }
    }
}

#[derive(Debug, Default)]
pub struct ScanSummary {
    pub images: usize,
    pub visited: usize,
    pub unreadable: usize,
    pub unsupported_names: usize,
    pub error: Option<std::io::ErrorKind>,
}

impl ScanSummary {
    /// A successful small listing needs no warning. Effects are always rows,
    /// so an empty directory must be reported explicitly instead of relying
    /// on the list panel's otherwise-unreachable empty state.
    pub(crate) fn message(&self, directory: &Path, listed_images: usize) -> String {
        let mut notes = Vec::new();
        if let Some(error) = self.error {
            let reason = match error {
                std::io::ErrorKind::NotFound => "folder not found",
                std::io::ErrorKind::PermissionDenied => "permission denied",
                std::io::ErrorKind::NotADirectory => "not a folder",
                _ => "directory unavailable",
            };
            notes.push(format!("Cannot read {}: {reason}", directory.display()));
        } else if listed_images == 0 {
            notes.push(format!("No images in {}", directory.display()));
        }
        if self.visited >= MAX_SCAN_ENTRIES {
            notes.push(format!("Scan limit reached ({MAX_SCAN_ENTRIES} entries)"));
        }
        if self.images > MAX_ENTRIES {
            notes.push(format!(
                "{} images found; search covers {listed_images} listed images",
                self.images
            ));
        }
        if self.unreadable > 0 {
            notes.push(format!("{} unreadable entries skipped", self.unreadable));
        }
        if self.unsupported_names > 0 {
            notes.push(format!(
                "{} unsupported filenames skipped",
                self.unsupported_names
            ));
        }
        notes.join(" · ")
    }
}

/// Resolve aliases off the event loop, keeping the listing's original paths.
/// Retain a valid current picture even if it falls outside the bounded listing.
pub fn resolve_selection(
    paths: &mut Vec<PathBuf>,
    current: &str,
    home: &Path,
    cancelled: impl Fn() -> bool,
) -> Option<(String, String)> {
    if current.is_empty() || is_effect_choice(current) || cancelled() {
        return None;
    }
    let expanded = expand_home(current, home);
    // Prefer the literal row over another symlink to the same image.
    let selected = paths
        .iter()
        .find(|path| **path == expanded)
        .cloned()
        .or_else(|| {
            let canonical = std::fs::canonicalize(&expanded).ok()?;
            paths
                .iter()
                .take_while(|_| !cancelled())
                .find(|path| std::fs::canonicalize(path).ok().as_ref() == Some(&canonical))
                .cloned()
        });
    let selected = match selected {
        Some(path) => path,
        None if !cancelled() && is_image(&expanded) && expanded.is_file() => {
            if paths.len() >= MAX_ENTRIES {
                paths.pop();
            }
            paths.push(expanded.clone());
            paths.sort_unstable_by(|a, b| sort_key(a).cmp(&sort_key(b)).then_with(|| a.cmp(b)));
            expanded
        }
        None => return None,
    };
    Some((current.to_owned(), selected.to_str()?.to_owned()))
}

/// What a directory entry's own type says about it, as the directory listing
/// reports it (`d_type`) rather than as a stat would.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// A regular file.
    File,
    /// A directory, socket, FIFO or device: never a wallpaper.
    Other,
    /// A symlink, or an entry whose type could not be read: only a stat that
    /// follows the link can tell whether a file is behind it.
    Unresolved,
}

impl EntryKind {
    /// Classify a [`std::fs::DirEntry::file_type`] result. On Linux that
    /// comes from `d_type` for free; `std` only falls back to an `lstat` on
    /// file systems that do not report entry types.
    #[must_use]
    pub fn of(file_type: std::io::Result<std::fs::FileType>) -> Self {
        match file_type {
            Ok(file_type) if file_type.is_file() => Self::File,
            Ok(file_type) if file_type.is_symlink() => Self::Unresolved,
            Ok(_) => Self::Other,
            Err(_) => Self::Unresolved,
        }
    }
}

/// Whether a path looks like an image this compositor can draw.
#[must_use]
pub fn is_image(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            IMAGE_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str())
        })
}

/// Where to look for wallpapers.
///
/// An explicit `wallpaper_dir` wins. Otherwise the directory holding the
/// current wallpaper is the best guess — whoever set one almost certainly
/// keeps the rest beside it — and failing that the usual pictures locations.
#[must_use]
pub fn resolve_directory(configured_dir: &str, current_wallpaper: &str, home: &Path) -> PathBuf {
    let configured = configured_dir.trim();
    if !configured.is_empty() {
        return expand_home(configured, home);
    }
    let current = current_wallpaper.trim();
    if !current.is_empty()
        && let Some(parent) = expand_home(current, home).parent()
        && parent.is_dir()
    {
        return parent.to_path_buf();
    }
    let wallpapers = home.join("Pictures").join("Wallpapers");
    if wallpapers.is_dir() {
        return wallpapers;
    }
    home.join("Pictures")
}

/// Expand a leading `~` against `home`; other paths are taken as given.
#[must_use]
pub fn expand_home(path: &str, home: &Path) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None if path == "~" => home.to_path_buf(),
        None => PathBuf::from(path),
    }
}

/// Image files in `dir`, sorted by name, capped at [`MAX_ENTRIES`].
///
/// Sorting is case-insensitive so `Alps.jpg` and `alps.jpg` do not end up at
/// opposite ends of the list.
#[cfg(test)]
#[must_use]
pub fn list_wallpapers(dir: &Path) -> Vec<PathBuf> {
    list_wallpapers_cancellable(dir, || false)
}

/// Cancellation is checked before every raw directory entry, not just images.
/// A blocked filesystem syscall itself cannot be interrupted by this flag.
#[must_use]
pub fn list_wallpapers_cancellable(dir: &Path, cancelled: impl Fn() -> bool) -> Vec<PathBuf> {
    scan_wallpapers_cancellable(dir, cancelled).0
}

/// Bounded scan with enough metadata to explain an incomplete picker.
#[must_use]
pub fn scan_wallpapers_cancellable(
    dir: &Path,
    cancelled: impl Fn() -> bool,
) -> (Vec<PathBuf>, ScanSummary) {
    let mut summary = ScanSummary::default();
    if cancelled() {
        return (Vec::new(), summary);
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => {
            summary.error = Some(error.kind());
            return (Vec::new(), summary);
        }
    };
    // Bound/check before filtering: millions of sidecars must remain cancellable.
    let candidates = bounded_scan(entries, &cancelled)
        .inspect(|_| summary.visited += 1)
        .filter_map(|entry| {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => {
                    summary.unreadable += 1;
                    return None;
                }
            };
            let path = entry.path();
            if !is_image(&path) {
                return None;
            }
            // Config and IPC paths are UTF-8 strings. A lossy replacement can
            // name another image, so never put an unrepresentable key in a row.
            if path.to_str().is_none() {
                summary.unsupported_names += 1;
                return None;
            }
            Some((path, EntryKind::of(entry.file_type())))
        });
    let (paths, images) = select_wallpapers_counted(candidates, Path::is_file);
    summary.images = images;
    (paths, summary)
}

fn bounded_scan<T>(
    entries: impl IntoIterator<Item = T>,
    cancelled: impl Fn() -> bool,
) -> impl Iterator<Item = T> {
    let mut entries = entries.into_iter();
    (0..MAX_SCAN_ENTRIES).map_while(move |_| if cancelled() { None } else { entries.next() })
}

/// The wallpapers among `entries`, sorted by name, capped at [`MAX_ENTRIES`].
///
/// The extension is checked first because it costs nothing. `is_file` — a
/// stat that follows symlinks — then runs only for images whose entry type
/// did not already settle the question, so a directory of forty thousand
/// photos is not forty thousand stats on the thread that opened the picker.
///
/// Each sort key is built once rather than per comparison. Batches retain
/// only the best [`MAX_ENTRIES`] names, bounding candidate storage even for
/// an enormous directory; only the final selection is fully ordered.
/// Equal keys (`Alps.jpg`
/// beside `alps.jpg`) fall back to the path, so the order never depends on
/// the order the directory happened to list them in.
#[must_use]
pub fn select_wallpapers(
    entries: impl IntoIterator<Item = (PathBuf, EntryKind)>,
    is_file: impl FnMut(&Path) -> bool,
) -> Vec<PathBuf> {
    select_wallpapers_counted(entries, is_file).0
}

fn select_wallpapers_counted(
    entries: impl IntoIterator<Item = (PathBuf, EntryKind)>,
    mut is_file: impl FnMut(&Path) -> bool,
) -> (Vec<PathBuf>, usize) {
    let candidates = entries
        .into_iter()
        .filter(|(path, kind)| {
            is_image(path)
                && path.to_str().is_some()
                && match kind {
                    EntryKind::File => true,
                    EntryKind::Other => false,
                    EntryKind::Unresolved => is_file(path),
                }
        })
        .map(|(path, _)| (sort_key(&path), path));
    let mut files = Vec::with_capacity(MAX_ENTRIES * 2);
    let mut images = 0;
    for candidate in candidates {
        images += 1;
        files.push(candidate);
        if files.len() == MAX_ENTRIES * 2 {
            // A discarded name already has MAX_ENTRIES better names, so
            // later entries can never bring it back into the final result.
            // Partition in batches to keep both storage and total ordering
            // work bounded without sorting every candidate on insertion.
            files.select_nth_unstable(MAX_ENTRIES);
            files.truncate(MAX_ENTRIES);
        }
    }
    if files.len() > MAX_ENTRIES {
        // Partition around the cap in linear time; only what survives the
        // cap is worth a full sort.
        files.select_nth_unstable(MAX_ENTRIES);
        files.truncate(MAX_ENTRIES);
    }
    files.sort_unstable();
    (files.into_iter().map(|(_, path)| path).collect(), images)
}

/// Search both the filename/path and the human-readable effect name. Tokens
/// are normalized once per query; a multiword search can match in any order.
pub(crate) fn matches_query(key: &str, label: &str, words: &[&str]) -> bool {
    if words.is_empty() {
        return true;
    }
    let key = key.to_lowercase();
    let label = label.to_lowercase();
    words
        .iter()
        .all(|word| key.contains(word) || label.contains(word))
}

/// Case-insensitive name a wallpaper is ordered by.
fn sort_key(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_lowercase()
}

/// One picker row: the file name, marked when it is the current wallpaper.
#[must_use]
pub fn picker_row(path: &Path, current: &str) -> String {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("(unnamed)");
    let marker = if path.to_str() == Some(current) {
        "\u{f00c}" // fa-check
    } else {
        " "
    };
    format!("\u{f03e} {marker} {name}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_matches_unicode_paths_and_all_query_words() {
        assert!(matches_query(
            "/walls/Été Alps.png",
            "Été Alps.png",
            &["été", "alps"]
        ));
        assert!(matches_query(
            SOLAR_SYSTEM_CHOICE,
            "Solar System 3D · drag to orbit",
            &["orbit", "solar"]
        ));
        assert!(!matches_query(
            "/walls/Alps.png",
            "Alps.png",
            &["alps", "beach"]
        ));
        assert!(matches_query("/walls/a.png", "a.png", &[]));
    }

    #[test]
    fn scan_summary_distinguishes_unavailable_empty_and_capped_folders() {
        let dir = temp_dir("scan-summary");
        let (empty, summary) = scan_wallpapers_cancellable(&dir, || false);
        assert!(empty.is_empty());
        assert!(summary.error.is_none());
        assert!(
            summary
                .message(&dir, empty.len())
                .starts_with("No images in ")
        );
        let (missing, summary) = scan_wallpapers_cancellable(&dir.join("absent"), || false);
        assert!(missing.is_empty());
        assert_eq!(summary.error, Some(std::io::ErrorKind::NotFound));
        assert!(summary.message(&dir, 0).contains("folder not found"));

        for index in 0..MAX_ENTRIES + 3 {
            std::fs::write(dir.join(format!("{index:04}.png")), b"x").unwrap();
        }
        std::fs::write(dir.join("notes.txt"), b"x").unwrap();
        std::fs::create_dir(dir.join("folder.png")).unwrap();
        let (paths, summary) = scan_wallpapers_cancellable(&dir, || false);
        assert_eq!(paths.len(), MAX_ENTRIES);
        assert_eq!(summary.images, MAX_ENTRIES + 3);
        assert_eq!(summary.visited, MAX_ENTRIES + 5);
        let message = summary.message(&dir, paths.len());
        assert!(message.contains("203 images found"));
        assert!(message.contains("search covers 200 listed images"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn scan_never_turns_non_utf8_filenames_into_the_wrong_image_path() {
        use std::os::unix::ffi::OsStringExt as _;
        let dir = temp_dir("scan-invalid-name");
        let invalid = dir.join(std::ffi::OsString::from_vec(b"bad\xff.png".to_vec()));
        let valid = dir.join("bad\u{fffd}.png");
        std::fs::write(&invalid, b"invalid name").unwrap();
        std::fs::write(&valid, b"valid name").unwrap();
        let (paths, summary) = scan_wallpapers_cancellable(&dir, || false);
        assert_eq!(paths, [valid]);
        assert_eq!(summary.images, 1);
        assert_eq!(summary.unsupported_names, 1);
        assert!(
            summary
                .message(&dir, 1)
                .contains("unsupported filenames skipped")
        );
        assert!(select_wallpapers([(invalid, EntryKind::File)], |_| true).is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn partial_scan_status_explains_both_limits_and_read_errors() {
        let summary = ScanSummary {
            images: MAX_ENTRIES + 1,
            visited: MAX_SCAN_ENTRIES,
            unreadable: 2,
            ..Default::default()
        };
        let message = summary.message(Path::new("/walls"), MAX_ENTRIES);
        assert!(message.contains("Scan limit reached (100000 entries)"));
        assert!(message.contains("201 images found"));
        assert!(message.contains("search covers 200 listed images"));
        assert!(message.contains("2 unreadable entries skipped"));
    }

    #[test]
    fn cancelled_scan_does_not_consume_even_one_raw_entry() {
        let cancellation = ScanCancellation::default();
        let cancelled = cancellation.observer();
        drop(cancellation);
        let entries =
            std::iter::from_fn(|| -> Option<()> { panic!("cancelled scan read an entry") });
        assert_eq!(bounded_scan(entries, cancelled).count(), 0);
    }

    #[test]
    fn scan_budget_counts_non_images_and_checks_cancellation_between_entries() {
        let visited = std::cell::Cell::new(0);
        let entries = std::iter::repeat_with(|| {
            visited.set(visited.get() + 1);
            "sidecar.txt"
        });
        assert_eq!(bounded_scan(entries, || false).count(), MAX_SCAN_ENTRIES);
        assert_eq!(visited.get(), MAX_SCAN_ENTRIES);
        visited.set(0);
        let entries = std::iter::repeat_with(|| {
            visited.set(visited.get() + 1);
            "sidecar.txt"
        });
        assert_eq!(bounded_scan(entries, || visited.get() >= 7).count(), 7);
        assert_eq!(visited.get(), 7);
    }

    #[test]
    fn selection_resolves_home_and_symlinks_without_rewriting_rows() {
        let dir = temp_dir("selection-alias");
        let real = dir.join("z-real.png");
        let alias = dir.join("a-alias.png");
        std::fs::write(&real, b"x").unwrap();
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let mut paths = vec![alias.clone(), real.clone()];
        let original = paths.clone();
        let selected = resolve_selection(&mut paths, "~/z-real.png", &dir, || false).unwrap();
        assert_eq!(selected.1, real.to_str().unwrap());
        assert_eq!(paths, original);
        paths.pop();
        let selected =
            resolve_selection(&mut paths, real.to_str().unwrap(), &dir, || false).unwrap();
        assert_eq!(selected.1, alias.to_str().unwrap());
        assert_eq!(paths, [alias]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn current_picture_survives_the_row_cap_without_growing_it() {
        let dir = temp_dir("selection-cap");
        let current = dir.join("z-current.png");
        std::fs::write(&current, b"x").unwrap();
        let mut paths: Vec<_> = (0..MAX_ENTRIES)
            .map(|i| dir.join(format!("{i:04}.png")))
            .collect();
        let selected =
            resolve_selection(&mut paths, current.to_str().unwrap(), &dir, || false).unwrap();
        assert_eq!(paths.len(), MAX_ENTRIES);
        assert_eq!(selected.1, current.to_str().unwrap());
        assert!(paths.contains(&current));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn selection_does_not_add_missing_effect_or_cancelled_paths() {
        let dir = temp_dir("selection-missing");
        let current = dir.join("a.png");
        let mut paths = Vec::new();
        assert!(resolve_selection(&mut paths, current.to_str().unwrap(), &dir, || false).is_none());
        std::fs::write(&current, b"x").unwrap();
        assert!(resolve_selection(&mut paths, current.to_str().unwrap(), &dir, || true).is_none());
        assert!(resolve_selection(&mut paths, ASTRONOMY_CHOICE, &dir, || false).is_none());
        assert!(paths.is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn solar_system_choice_is_selected_and_never_an_image() {
        use crate::jwm::features::SystemUiState;
        let config = crate::config::Config::default()
            .with_validated_value(
                "behavior.wallpaper_effect",
                &serde_json::json!("solar-system"),
            )
            .unwrap();
        assert_eq!(
            picker_current_at(config.behavior(), 1, 0),
            SOLAR_SYSTEM_CHOICE
        );
        let panel = SystemUiState::wallpaper_picker(&[], SOLAR_SYSTEM_CHOICE, "/walls");
        assert_eq!(panel.selected_wallpaper_choice(), Some(SOLAR_SYSTEM_CHOICE));
        assert_eq!(panel.selected_wallpaper(), None);
        assert!(is_effect_choice(ASTRONOMY_CHOICE));
        assert!(is_effect_choice(SOLAR_SYSTEM_CHOICE));
        assert!(!is_effect_choice("/walls/solar-system.jpg"));
    }

    #[test]
    fn astronomy_checks_resolved_texture_only_for_new_selection() {
        assert!(astronomy_texture_ready(false, "", |_| true));
        assert!(!astronomy_texture_ready(false, "", |_| false));
        assert!(!astronomy_texture_ready(false, "/missing.jpg", |_| false));
        assert!(astronomy_texture_ready(false, "/earth.jpg", |path| path == "/earth.jpg"));
        assert!(astronomy_texture_ready(true, "", |_| panic!(
            "active effect must be preserved"
        )));
    }

    #[test]
    fn astronomy_is_selected_without_becoming_an_image_preview_or_path() {
        let mut config = crate::config::Config::default();
        config
            .set_value("behavior.wallpaper_effect", &serde_json::json!("astronomy"))
            .unwrap();
        assert_eq!(
            picker_current_at(config.behavior(), 1, 100),
            ASTRONOMY_CHOICE
        );
        let paths = [PathBuf::from("/walls/static.png")];
        let mut panel = crate::jwm::features::SystemUiState::wallpaper_picker(
            &paths,
            ASTRONOMY_CHOICE,
            "/walls",
        );
        assert_eq!(panel.selected_wallpaper_choice(), Some(ASTRONOMY_CHOICE));
        assert_eq!(panel.selected_wallpaper(), None);
        panel.move_selection(-1);
        assert_eq!(panel.selected_wallpaper(), Some("/walls/static.png"));
        let picked = config
            .with_picker_wallpaper(panel.selected_wallpaper().unwrap(), 100)
            .unwrap();
        assert_eq!(picked.behavior().wallpaper_effect, "off");
        assert_eq!(picked.behavior().wallpaper, "/walls/static.png");
        let empty = crate::jwm::features::SystemUiState::wallpaper_picker(&[], "", "/missing");
        assert_eq!(empty.selected_wallpaper_choice(), Some(ASTRONOMY_CHOICE));
        assert_eq!(empty.selected_wallpaper(), None);
    }

    #[test]
    fn wallpaper_picker_reopens_on_todays_effective_choice_without_stopping_rotation() {
        let mut config = crate::config::Config::default();
        config
            .set_values(&[
                (
                    "behavior.wallpaper_rotation".into(),
                    serde_json::json!("daily"),
                ),
                (
                    "behavior.wallpaper_rotation_paths".into(),
                    serde_json::json!(["/walls/a.png", "/walls/b.png"]),
                ),
            ])
            .unwrap();
        assert_eq!(picker_current_at(config.behavior(), 1, 100), "/walls/a.png");
        let paths = [
            PathBuf::from("/walls/a.png"),
            PathBuf::from("/walls/picked.png"),
        ];
        let mut panel = crate::jwm::features::SystemUiState::wallpaper_picker(
            &paths,
            picker_current_at(config.behavior(), 1, 100),
            "/walls",
        );
        panel.move_selection(1);
        // Preview/navigation never touches the config; dropping this panel is
        // the model-level cancel operation.
        assert_eq!(picker_current_at(config.behavior(), 1, 100), "/walls/a.png");
        let picked = config
            .with_picker_wallpaper(panel.selected_wallpaper().unwrap(), 100)
            .unwrap();
        drop(panel);
        let reopened = crate::jwm::features::SystemUiState::wallpaper_picker(
            &paths,
            picker_current_at(picked.behavior(), 1, 100),
            "/walls",
        );
        assert_eq!(reopened.selected_wallpaper(), Some("/walls/picked.png"));
        assert_eq!(picker_current_at(picked.behavior(), 1, 101), "/walls/b.png");
        assert_eq!(picked.behavior().wallpaper_rotation, "daily");
        let source = include_str!("toggles.rs");
        assert_eq!(source.matches("with_picker_wallpaper(").count(), 1);
        assert!(source.contains("\"config/wallpaper\""));
        assert!(
            crate::ipc::IPC_REGISTRY
                .subscription_topics
                .contains(&"config")
        );
        assert_eq!(source.matches("self.current_picker_wallpaper()").count(), 2);
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jwm-wallpaper-{}-{tag}", std::process::id()));
        // Whatever an interrupted earlier run left behind would turn the
        // symlink fixtures into "already exists" failures.
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn image_extensions_are_recognized_case_insensitively() {
        assert!(is_image(Path::new("a.png")));
        assert!(is_image(Path::new("a.JPG")));
        assert!(is_image(Path::new("a.jpeg")));
        assert!(is_image(Path::new("a.WebP")));
        assert!(!is_image(Path::new("a.txt")));
        assert!(!is_image(Path::new("a")));
    }

    #[test]
    fn a_configured_directory_wins() {
        let home = Path::new("/home/ada");
        assert_eq!(
            resolve_directory("/srv/walls", "/home/ada/Pictures/x.png", home),
            PathBuf::from("/srv/walls")
        );
    }

    #[test]
    fn a_tilde_is_expanded_against_home() {
        let home = Path::new("/home/ada");
        assert_eq!(
            resolve_directory("~/walls", "", home),
            PathBuf::from("/home/ada/walls")
        );
        assert_eq!(expand_home("~", home), PathBuf::from("/home/ada"));
        assert_eq!(expand_home("/abs", home), PathBuf::from("/abs"));
    }

    #[test]
    fn without_configuration_the_current_wallpapers_directory_is_used() {
        let dir = temp_dir("current");
        let wallpaper = dir.join("now.png");
        std::fs::write(&wallpaper, b"x").expect("write");

        let resolved = resolve_directory("", wallpaper.to_str().unwrap(), Path::new("/home/ada"));
        assert_eq!(resolved, dir);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn listing_keeps_only_images_sorted_by_name() {
        let dir = temp_dir("list");
        for name in ["b.png", "A.jpg", "notes.txt", "c.webp"] {
            std::fs::write(dir.join(name), b"x").expect("write");
        }
        std::fs::create_dir_all(dir.join("subdir.png")).expect("dir");

        let names: Vec<String> = list_wallpapers(&dir)
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();

        // Case-insensitive order, no text file, no directory.
        assert_eq!(names, ["A.jpg", "b.png", "c.webp"]);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn listing_follows_symlinks_to_decide_what_is_a_file() {
        let dir = temp_dir("symlinks");
        std::fs::write(dir.join("real.png"), b"x").expect("write");
        std::fs::create_dir_all(dir.join("folder")).expect("dir");
        std::os::unix::fs::symlink(dir.join("real.png"), dir.join("linked.jpg")).expect("link");
        std::os::unix::fs::symlink(dir.join("folder"), dir.join("folder.png")).expect("link");
        std::os::unix::fs::symlink(dir.join("missing.png"), dir.join("dangling.png"))
            .expect("link");

        let names: Vec<String> = list_wallpapers(&dir)
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();

        // A link to a picture is a wallpaper; a link to a folder or to
        // nothing is not, whatever its name says.
        assert_eq!(names, ["linked.jpg", "real.png"]);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn only_images_the_entry_type_cannot_settle_are_stat_ed() {
        // Regression: the listing used to stat every entry, images or not,
        // before looking at the extension.
        let entries = vec![
            (PathBuf::from("/w/a.png"), EntryKind::File),
            (PathBuf::from("/w/notes.txt"), EntryKind::Unresolved),
            (PathBuf::from("/w/readme.txt"), EntryKind::File),
            (PathBuf::from("/w/folder.png"), EntryKind::Other),
            (PathBuf::from("/w/linked.jpg"), EntryKind::Unresolved),
            (PathBuf::from("/w/dangling.jpg"), EntryKind::Unresolved),
        ];
        let mut stats = Vec::new();
        let listed = select_wallpapers(entries, |path| {
            stats.push(path.to_path_buf());
            path.ends_with("linked.jpg")
        });

        assert_eq!(
            listed,
            [PathBuf::from("/w/a.png"), PathBuf::from("/w/linked.jpg")]
        );
        assert_eq!(
            stats,
            [
                PathBuf::from("/w/linked.jpg"),
                PathBuf::from("/w/dangling.jpg")
            ]
        );
    }

    #[test]
    fn the_cap_keeps_the_first_names_in_order() {
        let total = MAX_ENTRIES + 50;
        // Listed backwards, the way a directory may well hand them out.
        let entries: Vec<(PathBuf, EntryKind)> = (0..total)
            .rev()
            .map(|index| (PathBuf::from(format!("/w/{index:04}.png")), EntryKind::File))
            .collect();

        let listed = select_wallpapers(entries, |path| {
            unreachable!("{} has a known type", path.display())
        });

        let expected: Vec<PathBuf> = (0..MAX_ENTRIES)
            .map(|index| PathBuf::from(format!("/w/{index:04}.png")))
            .collect();
        assert_eq!(listed, expected);
    }

    #[test]
    fn names_that_differ_only_in_case_have_a_fixed_order() {
        for entries in [
            ["/w/b.png", "/w/alps.jpg", "/w/Alps.jpg"],
            ["/w/Alps.jpg", "/w/b.png", "/w/alps.jpg"],
        ] {
            let listed = select_wallpapers(
                entries.map(|path| (PathBuf::from(path), EntryKind::File)),
                |_| false,
            );
            assert_eq!(
                listed,
                [
                    PathBuf::from("/w/Alps.jpg"),
                    PathBuf::from("/w/alps.jpg"),
                    PathBuf::from("/w/b.png")
                ]
            );
        }
    }

    #[test]
    fn batched_selection_matches_global_order_across_batch_boundaries() {
        for total in [
            MAX_ENTRIES - 1,
            MAX_ENTRIES,
            MAX_ENTRIES * 2 - 1,
            MAX_ENTRIES * 2,
            MAX_ENTRIES * 2 + 1,
            MAX_ENTRIES * 13 + 57,
        ] {
            let paths: Vec<PathBuf> = (0..total)
                .map(|index| {
                    let name = if index % 2 == 0 { "Alps" } else { "alps" };
                    PathBuf::from(format!("/w/{index:05}/{name}{:03}.png", index % 97))
                })
                .collect();
            let mut expected = paths.clone();
            expected.sort_unstable_by(|a, b| sort_key(a).cmp(&sort_key(b)).then_with(|| a.cmp(b)));
            expected.truncate(MAX_ENTRIES);

            for reverse in [false, true] {
                let mut entries = paths.clone();
                if reverse {
                    entries.reverse();
                }
                let selected = select_wallpapers(
                    entries.into_iter().map(|path| (path, EntryKind::File)),
                    |_| unreachable!("regular files need no stat"),
                );
                assert_eq!(selected, expected, "total={total}, reverse={reverse}");
            }
        }
    }

    #[test]
    fn entry_types_map_to_what_a_stat_would_still_need_to_answer() {
        let dir = temp_dir("kinds");
        std::fs::write(dir.join("file.png"), b"x").expect("write");
        std::fs::create_dir_all(dir.join("folder")).expect("dir");
        std::os::unix::fs::symlink(dir.join("file.png"), dir.join("link.png")).expect("link");

        let kind = |name: &str| {
            let entry = std::fs::read_dir(&dir)
                .expect("read_dir")
                .flatten()
                .find(|entry| entry.file_name() == name)
                .expect("entry");
            EntryKind::of(entry.file_type())
        };
        assert_eq!(kind("file.png"), EntryKind::File);
        assert_eq!(kind("folder"), EntryKind::Other);
        assert_eq!(kind("link.png"), EntryKind::Unresolved);
        assert_eq!(
            EntryKind::of(Err(std::io::Error::other("no type"))),
            EntryKind::Unresolved
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_missing_directory_lists_nothing() {
        assert!(list_wallpapers(Path::new("/nonexistent/jwm/walls")).is_empty());
    }

    #[test]
    fn the_current_wallpaper_is_marked() {
        let path = Path::new("/walls/alps.jpg");
        assert!(picker_row(path, "/walls/alps.jpg").contains('\u{f00c}'));
        assert!(!picker_row(path, "/walls/other.jpg").contains('\u{f00c}'));
        assert!(picker_row(path, "").contains("alps.jpg"));
    }

    #[test]
    fn every_glyph_stays_in_the_widely_available_range() {
        // Same rule as the connectivity rows: FA5-era f6xx codepoints render
        // as hollow boxes in common Nerd Font builds.
        let row = picker_row(Path::new("/walls/a.jpg"), "/walls/a.jpg");
        for ch in row
            .chars()
            .filter(|ch| ('\u{f000}'..'\u{f900}').contains(ch))
        {
            assert!(
                (ch as u32) < 0xf600,
                "{ch:?} is outside the FontAwesome-4 range"
            );
        }
    }
}
