// Shared by all six Tauri hosts, including direct Cargo/Tauri CLI builds.
pub fn check_webkitgtk() {
    const CHECK: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../scripts/check-webkitgtk-version.sh"
    );
    println!("cargo:rerun-if-changed={CHECK}");
    for name in [
        "CARGO_CFG_TARGET_OS",
        "PKG_CONFIG",
        "PKG_CONFIG_PATH",
        "PKG_CONFIG_LIBDIR",
        "PKG_CONFIG_SYSROOT_DIR",
    ] {
        println!("cargo:rerun-if-env-changed={name}");
    }
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux") {
        return;
    }
    let status = std::process::Command::new("bash")
        .arg(CHECK)
        .status()
        .expect("could not run the WebKitGTK build requirement check");
    assert!(status.success(), "WebKitGTK build requirement check failed");
}
