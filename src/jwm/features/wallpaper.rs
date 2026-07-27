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

/// Extensions the compositor's image loader handles.
const IMAGE_EXTENSIONS: [&str; 6] = ["png", "jpg", "jpeg", "webp", "bmp", "gif"];

/// Most entries listed; a pictures directory can be enormous and the panel
/// scrolls, but reading tens of thousands of names would stall the frame that
/// opened it.
pub const MAX_ENTRIES: usize = 200;

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
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file() && is_image(path))
        .collect();
    files.sort_by_key(|path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_lowercase()
    });
    files.truncate(MAX_ENTRIES);
    files
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

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jwm-wallpaper-{}-{tag}", std::process::id()));
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
