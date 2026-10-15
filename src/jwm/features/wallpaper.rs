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

/// A fresh effect selection needs a local texture. Re-selecting the active
/// effect must not disable it merely because its source has since disappeared.
pub(crate) fn astronomy_texture_ready(
    already_active: bool,
    texture: &str,
    is_file: impl FnOnce(&Path) -> bool,
) -> bool {
    already_active || (!texture.trim().is_empty() && is_file(Path::new(texture)))
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
/// reads the whole directory. Only entries whose type is unresolved need a
/// stat, see [`select_wallpapers`].
pub const MAX_ENTRIES: usize = 200;

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
#[must_use]
pub fn list_wallpapers(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    // The extension is checked before the entry type is asked for: on a file
    // system without `d_type`, asking costs an lstat, and a pictures folder
    // full of sidecar files should not pay one per sidecar.
    let candidates = entries.flatten().filter_map(|entry| {
        let path = entry.path();
        is_image(&path).then(|| (path, EntryKind::of(entry.file_type())))
    });
    select_wallpapers(candidates, Path::is_file)
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
    mut is_file: impl FnMut(&Path) -> bool,
) -> Vec<PathBuf> {
    let candidates = entries
        .into_iter()
        .filter(|(path, kind)| {
            is_image(path)
                && match kind {
                    EntryKind::File => true,
                    EntryKind::Other => false,
                    EntryKind::Unresolved => is_file(path),
                }
        })
        .map(|(path, _)| (sort_key(&path), path));
    let mut files = Vec::with_capacity(MAX_ENTRIES * 2);
    for candidate in candidates {
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
    files.into_iter().map(|(_, path)| path).collect()
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
    fn astronomy_requires_a_regular_texture_only_for_new_selection() {
        assert!(!astronomy_texture_ready(false, "", |_| panic!(
            "empty path must not be inspected"
        )));
        assert!(!astronomy_texture_ready(false, "  ", |_| true));
        assert!(!astronomy_texture_ready(false, "/missing.jpg", |_| false));
        assert!(!astronomy_texture_ready(false, "/directory", |_| false));
        assert!(
            astronomy_texture_ready(false, "/earth.jpg", |path| path == Path::new("/earth.jpg"))
        );
        assert!(astronomy_texture_ready(true, "", |_| panic!(
            "active effect must be preserved"
        )));
        assert!(astronomy_texture_ready(true, "/gone.jpg", |_| false));
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
