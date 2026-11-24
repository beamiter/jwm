//! Native JWM applications: Cairo/Pango scenes on X11 and Wayland software surfaces.
use std::{
    path::{Path, PathBuf},
    process::ExitCode,
};

mod app;
mod editor;
mod find;
mod host;
pub mod notes;
mod preview;
mod theme;
mod ui;
pub mod viewer;

pub fn run(id: &str, name: &str, notes: bool) -> ExitCode {
    let mut paths = Vec::new();
    let mut literal = false;
    for argument in std::env::args_os().skip(1) {
        if !literal && argument == "--" {
            literal = true;
            continue;
        }
        if !literal && argument == "--version" {
            println!("{name} {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        if !literal && (argument == "--help" || argument == "-h") {
            println!(
                "Usage: {name} [--] [FILE…]\nNative JWM {}",
                if notes {
                    "notes and UTF-8 text editor"
                } else {
                    "image viewer"
                }
            );
            return ExitCode::SUCCESS;
        }
        paths.push(PathBuf::from(argument));
    }
    match host::run(id, notes, paths) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{name}: {error}");
            ExitCode::FAILURE
        }
    }
}

pub fn note_title(text: &str) -> String {
    text.lines()
        .find(|line| !line.trim().is_empty())
        .map(|line| line.trim().chars().take(48).collect())
        .unwrap_or_else(|| "未命名备忘录".into())
}

pub fn image_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "png"
                    | "jpg"
                    | "jpeg"
                    | "jpe"
                    | "gif"
                    | "webp"
                    | "bmp"
                    | "ico"
                    | "tif"
                    | "tiff"
                    | "pnm"
                    | "ppm"
                    | "pgm"
                    | "pbm"
                    | "pam"
            )
        })
}

pub fn neighbors(path: &Path) -> Vec<PathBuf> {
    neighbors_from(path, Path::new("."))
}

fn neighbors_from(path: &Path, current_dir: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<_> = path
        .parent()
        // A bare filename has an empty parent; reading it directly fails.
        .and_then(|parent| {
            std::fs::read_dir(current_dir.join(parent))
                .ok()
                .map(|entries| (parent, entries))
        })
        .into_iter()
        .flat_map(|(parent, entries)| {
            entries.filter_map(Result::ok).filter_map(move |entry| {
                let name = entry.file_name();
                (image_path(Path::new(&name)) && entry.path().is_file()).then(|| parent.join(name))
            })
        })
        .collect();
    // Always retain an explicitly opened image, including extensionless files.
    if !paths.iter().any(|candidate| candidate == path) {
        paths.push(path.to_path_buf());
    }
    paths.sort_by_cached_key(|path| {
        let name = path.file_name().unwrap_or_default();
        // Keep case-insensitive browsing and compare embedded numbers
        // naturally. The raw name breaks ties without changing any path.
        (
            gio::glib::FilenameCollationKey::from(name.to_string_lossy().to_lowercase()),
            name.to_owned(),
        )
    });
    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_title_uses_first_nonempty_line_and_bounds_characters() {
        assert_eq!(note_title("\n  购物清单  \n牛奶"), "购物清单");
        assert_eq!(note_title(" \n"), "未命名备忘录");
        assert_eq!(note_title(&"备".repeat(100)).chars().count(), 48);
    }

    #[test]
    fn image_types_match_supported_decoders() {
        assert!(image_path(Path::new("holiday.JPEG")));
        assert!(image_path(Path::new("photo.webp")));
        assert!(!image_path(Path::new("script.svg")));
        assert!(!image_path(Path::new("notes.txt")));
    }

    #[test]
    fn bare_image_path_discovers_siblings_without_duplicating_the_open_file() {
        let folder = std::env::temp_dir().join(format!("jwm-image-relative-{}", std::process::id()));
        std::fs::create_dir(&folder).unwrap();
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(folder.clone());
        for name in ["shot2.png", "shot10.png", "not-an-image.txt"] {
            std::fs::write(folder.join(name), b"").unwrap();
        }
        std::fs::create_dir(folder.join("album.png")).unwrap();

        assert_eq!(
            neighbors_from(Path::new("shot2.png"), &folder),
            vec![PathBuf::from("shot2.png"), PathBuf::from("shot10.png")]
        );
    }

    #[test]
    fn discovered_images_use_numeric_order_and_keep_original_paths() {
        use std::{fs, os::unix::ffi::OsStrExt};

        let folder = std::env::temp_dir().join(format!("jwm-image-order-{}", std::process::id()));
        fs::create_dir(&folder).unwrap();
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(folder.clone());
        let opened = folder.join("extensionless");
        for name in [
            "Shot10.png",
            "shot2.png",
            "Shot2.png",
            "shot1.png",
            "shot02.png",
            "截图10.png",
            "截图2.png",
            "截图1.png",
            "ignored.txt",
            "extensionless",
        ] {
            fs::write(folder.join(name), b"").unwrap();
        }
        let invalid_a = folder.join(std::ffi::OsStr::from_bytes(b"raw-\xfe.png"));
        let invalid_b = folder.join(std::ffi::OsStr::from_bytes(b"raw-\xff.png"));
        fs::write(&invalid_b, b"").unwrap();
        fs::write(&invalid_a, b"").unwrap();

        let ordered = neighbors(&opened);
        let pos = |name: &Path| ordered.iter().position(|path| path == name).unwrap();
        let index = |name: &str| pos(&folder.join(name));
        assert!(index("shot1.png") < index("shot2.png"));
        assert!(index("shot2.png") < index("Shot10.png"));
        assert!(index("Shot2.png") < index("shot2.png"));
        assert!(index("shot1.png") < index("shot02.png"));
        assert!(index("shot02.png") < index("Shot10.png"));
        assert!(index("截图1.png") < index("截图2.png"));
        assert!(index("截图2.png") < index("截图10.png"));
        assert!(pos(&invalid_a) < pos(&invalid_b));
        assert_eq!(ordered.iter().filter(|path| *path == &opened).count(), 1);
        assert!(!ordered.contains(&folder.join("ignored.txt")));
    }
}
