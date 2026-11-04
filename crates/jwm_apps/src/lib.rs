//! Native JWM applications: Cairo/Pango scenes on X11 and Wayland software surfaces.
use std::{
    path::{Path, PathBuf},
    process::ExitCode,
};

mod app;
mod editor;
mod host;
pub mod notes;
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
    let mut paths: Vec<_> = path
        .parent()
        .and_then(|parent| std::fs::read_dir(parent).ok())
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| image_path(path) && path.is_file())
        .collect();
    // Always retain an explicitly opened image, including extensionless files.
    if !paths.iter().any(|candidate| candidate == path) {
        paths.push(path.to_path_buf());
    }
    paths.sort_by_cached_key(|path| {
        path.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_lowercase()
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
}
