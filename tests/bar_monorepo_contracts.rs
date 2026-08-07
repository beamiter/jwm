//! Repository-level contracts for status bars that intentionally live outside
//! the root Cargo workspace.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

fn repository_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn bar_directories() -> BTreeSet<String> {
    fs::read_dir(repository_root().join("bars"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| !name.starts_with('.'))
        .collect()
}

fn installer_bars() -> BTreeSet<String> {
    let installer =
        fs::read_to_string(repository_root().join("scripts/install_jwm_scripts.sh")).unwrap();
    let (_, array) = installer
        .split_once("ALL_BARS=(")
        .expect("installer must define ALL_BARS");
    let (array, _) = array
        .split_once("\n)")
        .expect("installer ALL_BARS must be a multiline shell array");

    array
        .lines()
        .map(|line| line.split('#').next().unwrap_or_default().trim())
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

fn cargo_package_directories(root: &Path, packages: &mut BTreeSet<String>) {
    for entry in fs::read_dir(root).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if !entry.file_type().unwrap().is_dir() {
            continue;
        }

        let name = entry.file_name();
        let name = name.to_string_lossy();
        if matches!(name.as_ref(), "target" | "node_modules") || name.starts_with('.') {
            continue;
        }

        if path.join("Cargo.toml").is_file() {
            packages.insert(
                path.strip_prefix(repository_root())
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
            );
        }
        cargo_package_directories(&path, packages);
    }
}

fn excluded_bar_packages() -> BTreeSet<String> {
    let manifest = fs::read_to_string(repository_root().join("Cargo.toml")).unwrap();
    let manifest: toml::Value = toml::from_str(&manifest).unwrap();
    manifest["workspace"]["exclude"]
        .as_array()
        .expect("workspace.exclude must be an array")
        .iter()
        .filter_map(toml::Value::as_str)
        .filter(|path| path.starts_with("bars/"))
        .map(str::to_owned)
        .collect()
}

fn cargo_package_name(manifest: &Path) -> String {
    let manifest = fs::read_to_string(manifest).unwrap();
    let manifest: toml::Value = toml::from_str(&manifest).unwrap();
    manifest["package"]["name"]
        .as_str()
        .expect("bar Cargo package needs a name")
        .to_owned()
}

#[test]
fn every_bar_is_selectable_by_the_installer() {
    assert_eq!(
        installer_bars(),
        bar_directories(),
        "bars/ and scripts/install_jwm_scripts.sh::ALL_BARS must evolve together"
    );
}

#[test]
fn every_bar_cargo_package_is_explicitly_outside_the_root_workspace() {
    let mut packages = BTreeSet::new();
    cargo_package_directories(&repository_root().join("bars"), &mut packages);

    assert_eq!(
        excluded_bar_packages(),
        packages,
        "every Cargo package below bars/ needs an exact workspace.exclude entry"
    );
}

#[test]
fn every_selectable_bar_has_an_installable_executable_entrypoint() {
    for bar in installer_bars() {
        let directory = repository_root().join("bars").join(&bar);
        let tauri = directory.join("src-tauri");

        if tauri.join("Cargo.toml").is_file() {
            assert!(
                tauri.join("tauri.conf.json").is_file(),
                "{bar} needs src-tauri/tauri.conf.json"
            );
            assert!(
                tauri.join("src/main.rs").is_file(),
                "{bar} needs an executable Tauri target"
            );
            assert_eq!(
                cargo_package_name(&tauri.join("Cargo.toml")),
                bar,
                "the Tauri package name is also its installed executable name"
            );
            assert!(
                directory.join("package.json").is_file()
                    || (directory.join("Cargo.toml").is_file()
                        && directory.join("Trunk.toml").is_file()),
                "{bar} needs either a package-manager frontend or Cargo + Trunk"
            );
        } else {
            assert!(
                directory.join("Cargo.toml").is_file(),
                "{bar} needs a Cargo.toml build entrypoint"
            );
            assert_eq!(
                cargo_package_name(&directory.join("Cargo.toml")),
                bar,
                "a native bar's package name is also its installed executable name"
            );
        }
    }
}

#[test]
fn tauri_install_path_declares_and_checks_its_linux_prerequisites() {
    let root = repository_root();
    let bootstrap = fs::read_to_string(root.join("scripts/bootstrap_deps.sh")).unwrap();
    let installer = fs::read_to_string(root.join("scripts/install_jwm_scripts.sh")).unwrap();
    let readme = fs::read_to_string(root.join("README.md")).unwrap();

    assert!(
        bootstrap.contains("--with-tauri") && bootstrap.contains("libwebkit2gtk-4.1-dev"),
        "the optional bootstrap must install Tauri 2's Linux webview dependency"
    );
    assert!(
        installer.contains("pkg-config --exists webkit2gtk-4.1"),
        "the installer must fail early when WebKitGTK 4.1 is unavailable"
    );
    assert!(
        readme.contains("bootstrap_deps.sh --with-tauri"),
        "the documented Tauri path must enable its optional system dependencies"
    );
}
