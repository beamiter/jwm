// Emit an rpath to the privately-installed PipeWire 1.2 lib dir, scoped to the
// jwm-portal binary only. Avoids needing LD_LIBRARY_PATH when xdg-desktop-portal
// activates this service via D-Bus, and avoids polluting other workspace
// binaries with a stray rpath.

fn main() {
    let pw_lib = "/opt/pipewire-1.2/lib";
    println!("cargo:rustc-link-arg-bin=jwm-portal=-Wl,-rpath,{pw_lib}");
    println!("cargo:rerun-if-changed=build.rs");
}
