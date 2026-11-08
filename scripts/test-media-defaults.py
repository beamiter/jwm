#!/usr/bin/env python3
"""Check session-specific application defaults without installing on the host."""
import configparser
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
SOURCE = (ROOT / "scripts/install-jwm.sh").read_text()
START = SOURCE.index("install_jwm_media_defaults() {")
END = SOURCE.index("\ninstall_bundled_wallpapers() {", START)
PROGRAM = """set -euo pipefail
id() { printf '1000\\n'; }
ok() { :; }
jwm_config_dir() { printf '%s/jwm\\n' "$XDG_CONFIG_HOME"; }
""" + SOURCE[START:END] + "\ninstall_jwm_media_defaults\n"


def parse(text):
    config = configparser.ConfigParser(interpolation=None)
    config.optionxform = str
    config.read_string(text)
    return config


with tempfile.TemporaryDirectory(prefix="jwm-media-test-") as directory:
    root = Path(directory)
    config_home = root / "config space"
    apps = root / "data/applications"
    binary_dir = root / "bin"
    for path in (config_home, apps, binary_dir):
        path.mkdir(parents=True)
    for name in ("jwm-player", "jwm-notes", "jwm-image-viewer"):
        binary = binary_dir / name
        binary.write_text("#!/bin/sh\nexit 0\n")
        binary.chmod(0o755)
    expected = {}
    desktops = [*(ROOT / "bridge/dist").glob("*.desktop"),
                *(ROOT / "crates/jwm_apps/dist").glob("*.desktop")]
    for desktop in desktops:
        desktop_id = desktop.name
        shutil.copy2(desktop, apps / desktop_id)
        for mime in parse(desktop.read_text())["Desktop Entry"]["MimeType"].split(";"):
            if mime:
                expected[mime] = desktop_id + ";"
    (apps / "original.desktop").write_text(
        "[Desktop Entry]\nType=Application\nName=Original player\n"
        "Exec=/bin/true %F\nMimeType=" + ";".join(expected) + ";\n"
    )
    assert "audio/vnd.wave" in expected, "canonical WAV MIME must be registered"
    shared = config_home / "mimeapps.list"
    original = "# User defaults shared with Ubuntu\n[Default Applications]\n" + "".join(
        f"{mime}=original.desktop;\n" for mime in expected
    )
    shared.write_text(original)
    target = config_home / "jwm-mimeapps.list"
    existing = "# Existing JWM preferences\n[Default Applications]\napplication/pdf=reader.desktop;\ntext/plain=editor.desktop;\naudio/mpeg=original.desktop;\n"
    target.write_text(existing)
    target.chmod(0o640)
    env = dict(os.environ, PROJECT_ROOT=str(ROOT), XDG_CONFIG_HOME=str(config_home),
               XDG_CONFIG_DIRS=str(root / "system-config"), XDG_DATA_HOME=str(apps.parent),
               XDG_DATA_DIRS="/usr/share", HOME=str(root), SUDO_USER="", LC_ALL="C",
               PATH=str(binary_dir) + os.pathsep + os.environ["PATH"])

    def install():
        return subprocess.run(["bash", "-c", PROGRAM], env=env, capture_output=True, text=True)

    result = install()
    assert result.returncode == 0, result.stderr
    defaults = parse(target.read_text())["Default Applications"]
    assert all(defaults[mime] == desktop_id for mime, desktop_id in expected.items())
    assert defaults["text/plain"] == "org.jwm.Notes.desktop;"
    assert defaults["application/pdf"] == "reader.desktop;"
    assert shared.read_text() == original
    assert target.stat().st_mode & 0o777 == 0o640
    backup = target.with_suffix(".list.bak")
    assert backup.read_text() == existing
    revision = target.stat().st_mtime_ns
    assert install().returncode == 0
    assert target.stat().st_mtime_ns == revision
    assert backup.read_text() == existing
    assert not list(config_home.glob(".jwm-mimeapps-*"))
    print("MIME coverage, unrelated defaults, backup and repeat install: PASS")

    if shutil.which("gio") and shutil.which("xdg-mime"):
        for session in ("jwm", "JWM", "ubuntu:GNOME", "GNOME"):
            session_env = dict(env, XDG_CURRENT_DESKTOP=session)
            for mime in ("audio/mpeg", "audio/vnd.wave", "audio/x-wav", "audio/x-wavpack",
                         "audio/x-ms-wma", "audio/mp4", "video/mp4", "video/x-matroska",
                         "text/plain", "text/markdown", "image/png", "image/jpeg", "image/webp", "image/tiff"):
                desktop_id = expected[mime].rstrip(";") if session.lower() == "jwm" else "original.desktop"
                gio = subprocess.run(["gio", "mime", mime], env=session_env, capture_output=True, text=True, check=True)
                assert gio.stdout.splitlines()[0].endswith(": " + desktop_id), (session, mime, gio.stdout)
                xdg = subprocess.run(["xdg-mime", "query", "default", mime], env=session_env, capture_output=True, text=True, check=True)
                assert xdg.stdout.strip() == desktop_id, (session, mime, xdg.stdout)
        print("GIO and xdg-mime defaults in JWM and Ubuntu/GNOME: PASS")
    else:
        print("GIO/xdg-mime integration: SKIP (tools unavailable)")

    # Exercise the actual user installer with private build artifacts. In
    # particular, older xdg-utils cannot resolve unnecessarily quoted Exec
    # paths, even though GIO accepts those entries.
    # Only this mocked-build fixture supplies development metadata; the real
    # MIME queries above and the production installer's checks stay intact.
    pkg_config_log = root / "native-pkg-config.log"
    cargo_log = root / "native-cargo.log"
    pkg_config = binary_dir / "pkg-config"
    pkg_config.write_text('''#!/usr/bin/env bash
set -euo pipefail
printf '%s\\n' "$*" >> "$NATIVE_INSTALL_PKG_CONFIG_LOG"
[[ $# == 4 ]] || exit 90
[[ $1 == --exists && $2 == cairo && $3 == pangocairo && $4 == gio-2.0 ]] || exit 90
for dependency in "${@:2}"; do
    [[ $dependency != "${NATIVE_INSTALL_MISSING_DEPENDENCY:-}" ]] || exit 1
done
''')
    pkg_config.chmod(0o755)
    cargo = binary_dir / "cargo"
    cargo.write_text('''#!/usr/bin/env bash
set -euo pipefail
printf '%s\\n' "$*" >> "$NATIVE_INSTALL_CARGO_LOG"
target= profile=debug
while (($#)); do
    case "$1" in
        --target-dir) target=$2; shift 2 ;;
        --release) profile=release; shift ;;
        *) shift ;;
    esac
done
mkdir -p "$target/$profile"
for name in jwm-notes jwm-image-viewer; do
    printf '#!/bin/sh\\nexit 0\\n' > "$target/$profile/$name"
    chmod +x "$target/$profile/$name"
done
''')
    cargo.chmod(0o755)
    native_env = dict(env, CARGO_TARGET_DIR=str(root / "build"),
                      NATIVE_INSTALL_PKG_CONFIG_LOG=str(pkg_config_log),
                      NATIVE_INSTALL_CARGO_LOG=str(cargo_log),
                      NATIVE_INSTALL_MISSING_DEPENDENCY="")
    result = subprocess.run(["bash", str(ROOT / "scripts/install-native-apps.sh"), "--debug"],
                            env=dict(native_env, NATIVE_INSTALL_MISSING_DEPENDENCY="pangocairo"),
                            capture_output=True, text=True)
    assert result.returncode == 1, (result.stdout, result.stderr)
    assert "Cairo/Pango/GLib development files are required" in result.stderr
    assert pkg_config_log.read_text().splitlines() == ["--exists cairo pangocairo gio-2.0"]
    assert not cargo_log.exists(), "missing native dependency reached Cargo"
    assert not (root / ".local/bin").exists(), "missing dependency installed native applications"
    assert shared.read_text() == original and target.stat().st_mtime_ns == revision
    print("Missing native dependency fails before building or installing: PASS")
    result = subprocess.run(["bash", str(ROOT / "scripts/install-native-apps.sh"), "--debug"],
                            env=native_env,
                            capture_output=True, text=True)
    assert result.returncode == 0, result.stderr
    assert pkg_config_log.read_text().splitlines() == ["--exists cairo pangocairo gio-2.0"] * 2
    assert len(cargo_log.read_text().splitlines()) == 1
    assert shared.read_text() == original
    assert (root / ".local/bin/jwm-notes").is_file()
    assert (root / ".local/bin/jwm-image-viewer").is_file()
    if shutil.which("gio") and shutil.which("xdg-mime"):
        for mime in ("text/plain", "image/png", "image/webp"):
            expected_id = expected[mime].rstrip(";")
            session_env = dict(env, XDG_CURRENT_DESKTOP="jwm")
            value = subprocess.check_output(["xdg-mime", "query", "default", mime], env=session_env, text=True).strip()
            assert value == expected_id, (mime, value)
    print("User-only installation and xdg-utils Exec compatibility: PASS")

    target.unlink()
    assert install().returncode == 0
    assert shared.read_text() == original
    linked = config_home / "linked-mimeapps.list"
    target.rename(linked)
    target.symlink_to(linked)
    assert install().returncode != 0
    assert target.is_symlink()
    print("Fresh install and symlink preservation: PASS")

for path in ROOT.glob("jwm-*.desktop"):
    session = parse(path.read_text())["Desktop Entry"]
    assert "XDG_CURRENT_DESKTOP=jwm" in session["Exec"], path
    assert session["DesktopNames"] == "JWM", path
print("X11/Wayland session identity: PASS")
