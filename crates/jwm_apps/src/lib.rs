use gtk::{gio, glib, prelude::*};
use std::path::{Path, PathBuf};

pub mod notes;
pub mod viewer;

pub fn run(id: &str, name: &str, notes: bool) -> glib::ExitCode {
    if std::env::args_os().any(|arg| arg == "--version") {
        println!("{name} {}", env!("CARGO_PKG_VERSION"));
        return glib::ExitCode::SUCCESS;
    }
    if std::env::args_os().any(|arg| arg == "--help" || arg == "-h") {
        println!(
            "Usage: {name} [--] [FILE…]\nNative JWM {}",
            if notes {
                "notes and UTF-8 text editor"
            } else {
                "image viewer"
            }
        );
        return glib::ExitCode::SUCCESS;
    }
    glib::set_prgname(Some(name));
    glib::set_application_name(if notes {
        "JWM 备忘录"
    } else {
        "JWM 图片查看器"
    });
    let app = gtk::Application::builder()
        .application_id(id)
        .flags(gio::ApplicationFlags::HANDLES_OPEN)
        .build();
    app.connect_activate(move |app| {
        if let Some(window) = app.active_window() {
            window.present();
        } else if notes {
            notes::open(app, None);
        } else {
            viewer::open(app, vec![]);
        }
    });
    app.connect_open(move |app, files, _| {
        if notes {
            for file in files {
                notes::open(app, Some(file.clone()));
            }
        } else {
            viewer::open(app, files.to_vec());
        }
    });
    app.run()
}

pub(crate) fn button(icon: &str, tooltip: &str, action: &str) -> gtk::Button {
    gtk::Button::builder()
        .icon_name(icon)
        .tooltip_text(tooltip)
        .action_name(action)
        .build()
}

pub(crate) fn action(
    window: &gtk::ApplicationWindow,
    name: &str,
    keys: &[&str],
    callback: impl Fn() + 'static,
) {
    let action = gio::SimpleAction::new(name, None);
    action.connect_activate(move |_, _| callback());
    window.add_action(&action);
    if let Some(app) = window.application() {
        app.set_accels_for_action(&format!("win.{name}"), keys);
    }
}

pub(crate) fn error(window: &impl IsA<gtk::Window>, message: &str) {
    gtk::AlertDialog::builder()
        .message("操作未完成")
        .detail(message)
        .build()
        .show(Some(window));
}

pub(crate) fn local_path(file: &gio::File) -> Result<PathBuf, String> {
    file.path().ok_or_else(|| "请选择本地文件。".to_string())
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
