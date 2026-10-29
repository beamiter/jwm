use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../.git/HEAD");
    if let Ok(output) = Command::new("git").args(["rev-parse", "HEAD"]).output()
        && output.status.success()
        && let Ok(revision) = String::from_utf8(output.stdout)
    {
        println!(
            "cargo:rustc-env=JWM_EVA_SOURCE_REVISION={}+worktree",
            revision.trim()
        );
    }
    println!("cargo:rerun-if-env-changed=JWM_PIPEWIRE_PREFIX");
    println!("cargo:rerun-if-env-changed=PKG_CONFIG_PATH");

    // System PipeWire installations need no rpath. A developer-provided
    // prefix is opt-in and scoped to the Eva binary so it remains usable
    // when D-Bus activates it without LD_LIBRARY_PATH.
    let Some(prefix) = std::env::var_os("JWM_PIPEWIRE_PREFIX").filter(|value| !value.is_empty())
    else {
        return;
    };
    let prefix = PathBuf::from(prefix);
    // pkg-config knows whether this prefix uses lib, lib64, or a multiarch
    // directory. Keep rpath aligned with the library actually used to link.
    let configured_lib = Command::new("pkg-config")
        .args(["--variable=libdir", "libpipewire-0.3"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|path| PathBuf::from(path.trim()))
        .filter(|path| path.starts_with(&prefix) && path.is_dir());
    let lib = prefix.join("lib");
    let lib64 = prefix.join("lib64");
    let library_dir = configured_lib.unwrap_or_else(|| {
        if lib.is_dir() || !lib64.is_dir() {
            lib
        } else {
            lib64
        }
    });
    println!("cargo:rustc-link-search=native={}", library_dir.display());
    println!("cargo:rustc-link-arg=-Wl,-rpath,{}", library_dir.display());
}
