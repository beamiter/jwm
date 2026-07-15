#!/usr/bin/env python3
from pathlib import Path


def replace_once(path: str, old: str, new: str, label: str) -> None:
    file = Path(path)
    text = file.read_text()
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one match, found {count}")
    file.write_text(text.replace(old, new, 1))


replace_once(
    "README.md",
    "cargo build --release\ncargo test --lib --bins --tests",
    "cargo build --locked --release\ncargo test --locked --lib --bins --tests",
    "README locked commands",
)

contributing = Path("CONTRIBUTING.md")
text = contributing.read_text()
for old, new, label in [
    ("cargo build\n", "cargo build --locked\n", "contributing build"),
    ("cargo check --all-targets", "cargo check --locked --all-targets", "contributing check"),
    (
        "cargo clippy --all-targets --no-deps",
        "cargo clippy --locked --all-targets --no-deps",
        "contributing clippy",
    ),
    (
        "cargo test --lib --bins --tests",
        "cargo test --locked --lib --bins --tests",
        "contributing tests",
    ),
    (
        "cargo run -- --backend wayland-winit --doctor\ncargo run -- --backend wayland-winit",
        "cargo run --locked -- --backend wayland-winit --doctor\ncargo run --locked -- --backend wayland-winit",
        "contributing nested run",
    ),
]:
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one match, found {count}")
    text = text.replace(old, new, 1)
contributing.write_text(text)

replace_once(
    "scripts/install_jwm_scripts.sh",
    "cargo build $CARGO_BUILD_MODE_FLAG $CARGO_JOBS",
    "cargo build --locked $CARGO_BUILD_MODE_FLAG $CARGO_JOBS",
    "main installer locked build",
)
replace_once(
    "scripts/install-portal.sh",
    'cargo build --release --target-dir "$PORTAL_TARGET_DIR" --manifest-path "$PORTAL_MANIFEST"',
    'cargo build --locked --release --target-dir "$PORTAL_TARGET_DIR" --manifest-path "$PORTAL_MANIFEST"',
    "portal installer locked build",
)
replace_once(
    "scripts/run_nested.sh",
    "cargo build $build_flag --bin jwm",
    "cargo build --locked $build_flag --bin jwm",
    "nested runner locked build",
)

jwm_tool = Path("tools/jwm_tool.rs")
text = jwm_tool.read_text()
for old, new, label in [
    (
        '''    let status = Command::new("cargo")
        .arg("build")
        .arg("--release")
''',
        '''    let status = Command::new("cargo")
        .arg("build")
        .arg("--locked")
        .arg("--release")
''',
        "jwm-tool locked rebuild",
    ),
    (
        '''        InstallPlanEntry {
            name: "jwm-tool",
            source: jwm_dir.join("target/release/jwm-tool"),
            destination_dir: "/usr/local/bin/",
            mode: "0755",
        },
        InstallPlanEntry {
            name: "jwm-x11rb.desktop",
''',
        '''        InstallPlanEntry {
            name: "jwm-tool",
            source: jwm_dir.join("target/release/jwm-tool"),
            destination_dir: "/usr/local/bin/",
            mode: "0755",
        },
        InstallPlanEntry {
            name: "jwm-support",
            source: jwm_dir.join("target/release/jwm-support"),
            destination_dir: "/usr/local/bin/",
            mode: "0755",
        },
        InstallPlanEntry {
            name: "jwm-x11rb.desktop",
''',
        "jwm-tool support install plan",
    ),
    (
        '    println!("安装 JWM 与 jwm-tool...");',
        '    println!("安装 JWM、jwm-tool 与 jwm-support...");',
        "jwm-tool install message",
    ),
    (
        '''    let status = Command::new("sudo")
        .args(["rm", "-f", "/usr/local/bin/jwm", "/usr/local/bin/jwm-tool"])
''',
        '''    let status = Command::new("sudo")
        .args([
            "rm",
            "-f",
            "/usr/local/bin/jwm",
            "/usr/local/bin/jwm-tool",
            "/usr/local/bin/jwm-support",
        ])
''',
        "jwm-tool legacy cleanup",
    ),
    (
        '''                InstallPlanEntry {
                    name: "jwm-tool",
                    source: PathBuf::from("/src/jwm/target/release/jwm-tool"),
                    destination_dir: "/usr/local/bin/",
                    mode: "0755",
                },
                InstallPlanEntry {
                    name: "jwm-x11rb.desktop",
''',
        '''                InstallPlanEntry {
                    name: "jwm-tool",
                    source: PathBuf::from("/src/jwm/target/release/jwm-tool"),
                    destination_dir: "/usr/local/bin/",
                    mode: "0755",
                },
                InstallPlanEntry {
                    name: "jwm-support",
                    source: PathBuf::from("/src/jwm/target/release/jwm-support"),
                    destination_dir: "/usr/local/bin/",
                    mode: "0755",
                },
                InstallPlanEntry {
                    name: "jwm-x11rb.desktop",
''',
        "jwm-tool install plan test",
    ),
]:
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one match, found {count}")
    text = text.replace(old, new, 1)
jwm_tool.write_text(text)
