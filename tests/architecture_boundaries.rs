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

    let files = if root.is_file() {
        vec![root.clone()]
    } else {
        rust_files_below(&root)
    };
    for path in files {
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

// Platform-neutral algorithms live in `backend::compositor_common`. The X11
// facade re-exports them for its own tree, but policy code and the Wayland
// backends must depend on the canonical location, not the X11 namespace.
#[test]
fn policy_and_wayland_do_not_reach_into_the_x11_tree_for_shared_algorithms() {
    for root in [
        "src/jwm.rs",
        "src/jwm",
        "src/core",
        "src/backend/wayland_udev",
        "src/backend/wayland_x11",
        "src/backend/wayland_winit",
    ] {
        assert_files_exclude(root, &["x11::compositor_common"]);
    }
}

// The window-management policy layer talks to platforms through
// `backend::api` capabilities. Concrete backend types may appear only for
// documented downcasts (`backend::x11rb`, `backend::xcb`, `backend::wayland*`
// in process/launcher glue); the shared X11 implementation tree is never a
// legitimate policy dependency.
#[test]
fn jwm_policy_does_not_import_the_shared_x11_implementation() {
    assert_files_exclude("src/jwm.rs", &["backend::x11::"]);
    assert_files_exclude("src/jwm", &["backend::x11::"]);
}
