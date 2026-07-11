//! Lightweight dependency-rule checks for boundaries that Rust's module
//! visibility cannot express on its own.

use std::fs;
use std::path::{Path, PathBuf};

fn rust_files_below(root: &Path) -> Vec<PathBuf> {
    let mut pending = vec![root.to_path_buf()];
    let mut files = Vec::new();

    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                files.push(path);
            }
        }
    }
    files
}

fn assert_files_exclude(root: &str, forbidden: &[&str]) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join(root);
    let mut violations = Vec::new();

    for path in rust_files_below(&root) {
        let source = fs::read_to_string(&path).unwrap();
        for dependency in forbidden {
            if source.contains(dependency) {
                violations.push(format!("{} imports {dependency}", path.display()));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "architecture boundary violations:\n{}",
        violations.join("\n")
    );
}

#[test]
fn platform_backends_do_not_depend_on_jwm_policy() {
    assert_files_exclude("src/backend", &["crate::jwm", "super::super::jwm"]);
}

#[test]
fn core_does_not_depend_on_concrete_platforms() {
    assert_files_exclude(
        "src/core",
        &[
            "backend::x11",
            "backend::x11rb",
            "backend::xcb",
            "backend::wayland_udev",
            "backend::wayland_x11",
            "backend::wayland_winit",
        ],
    );
}
