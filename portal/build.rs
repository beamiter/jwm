use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=JWM_PIPEWIRE_PREFIX");

    // System PipeWire installations need no rpath. A developer-provided
    // prefix is opt-in and scoped to the portal binary so it remains usable
    // when D-Bus activates it without LD_LIBRARY_PATH.
    let Some(prefix) = std::env::var_os("JWM_PIPEWIRE_PREFIX").filter(|value| !value.is_empty())
    else {
        return;
    };
    let prefix = PathBuf::from(prefix);
    let lib = prefix.join("lib");
    let lib64 = prefix.join("lib64");
    let library_dir = if lib.is_dir() || !lib64.is_dir() {
        lib
    } else {
        lib64
    };
    println!("cargo:rustc-link-search=native={}", library_dir.display());
    println!(
        "cargo:rustc-link-arg-bin=jwm-portal=-Wl,-rpath,{}",
        library_dir.display()
    );
}
