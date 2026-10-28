#!/usr/bin/env python3
"""Check session-specific player defaults without installing on the host."""
import configparser
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
SOURCE = (ROOT / "scripts/install_jwm_scripts.sh").read_text()
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
    player = binary_dir / "jwm-player"
    player.write_text("#!/bin/sh\nexit 0\n")
    player.chmod(0o755)
    expected = {}
    for desktop_id in ("jwm-player.desktop", "jwm-video.desktop"):
        desktop = ROOT / "bridge/dist" / desktop_id
        shutil.copy2(desktop, apps / desktop_id)
        for mime in parse(desktop.read_text())["Desktop Entry"]["MimeType"].split(";"):
            if mime:
                expected[mime] = desktop_id + ";"
    (apps / "original.desktop").write_text(
        "[Desktop Entry]\nType=Application\nName=Original player\n"
        "Exec=/bin/true %F\nMimeType=" + ";".join(expected) + ";\n"
    )
    shared = config_home / "mimeapps.list"
    original = "# User defaults shared with Ubuntu\n[Default Applications]\n" + "".join(
        f"{mime}=original.desktop;\n" for mime in expected
    )
    shared.write_text(original)
    target = config_home / "jwm-mimeapps.list"
    existing = "# Existing JWM preferences\n[Default Applications]\ntext/plain=editor.desktop;\naudio/mpeg=original.desktop;\n"
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
    assert defaults["text/plain"] == "editor.desktop;"
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
            for mime in ("audio/mpeg", "video/mp4", "video/x-matroska"):
                desktop_id = expected[mime].rstrip(";") if session.lower() == "jwm" else "original.desktop"
                gio = subprocess.run(["gio", "mime", mime], env=session_env, capture_output=True, text=True, check=True)
                assert gio.stdout.splitlines()[0].endswith(": " + desktop_id), (session, mime, gio.stdout)
                xdg = subprocess.run(["xdg-mime", "query", "default", mime], env=session_env, capture_output=True, text=True, check=True)
                assert xdg.stdout.strip() == desktop_id, (session, mime, xdg.stdout)
        print("GIO and xdg-mime defaults in JWM and Ubuntu/GNOME: PASS")
    else:
        print("GIO/xdg-mime integration: SKIP (tools unavailable)")

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
