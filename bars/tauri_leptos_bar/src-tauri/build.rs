#[path = "../../tauri_build.rs"]
mod requirements;

fn main() {
    requirements::check_webkitgtk();
    tauri_build::build()
}
