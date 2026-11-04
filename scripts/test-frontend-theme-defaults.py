#!/usr/bin/env python3
"""Fresh frontend defaults inherit; existing user overrides remain byte-exact."""

import os
from pathlib import Path
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
source = (ROOT / "scripts/install-jwm.sh").read_text()
start = source.index("install_frontend_theme_defaults() {")
end = source.index(
    "\n# ============================================================", start
)
function = source[start:end]

with tempfile.TemporaryDirectory(prefix="jwm-frontend-theme-") as temporary:
    root = Path(temporary)

    def run(name, initial=None, legacy=False, custom=False, link=False, fifo=False):
        home = root / name / "home"
        config = root / name / "config space"
        home.mkdir(parents=True)
        (config / "xbar").mkdir(parents=True)
        destination = config / "xbar/config.toml"
        original = root / name / "original.toml"
        if initial is not None:
            if link:
                original.write_text(initial)
                destination.symlink_to(original)
            else:
                destination.write_text(initial)
            (original if link else destination).chmod(0o640)
        if fifo:
            os.mkfifo(destination)
        if legacy:
            saved = home / ".config/xilem_bar/theme"
            saved.parent.mkdir(parents=True)
            saved.write_text("dark\n")
        env = dict(
            os.environ,
            HOME=str(home),
            TEST_HOME=str(home),
            TEST_CONFIG_HOME=str(config),
            SUDO_USER="",
            XBAR_CONFIG=str(root / "custom.toml") if custom else "",
        )
        program = (
            "set -euo pipefail\ninfo(){ :; }\nid(){ echo 1000; }\n"
            'target_user_home(){ printf "%s\\n" "$TEST_HOME"; }\n'
            'jwm_config_dir(){ printf "%s/jwm\\n" "$TEST_CONFIG_HOME"; }\n'
            + function
            + "\ninstall_frontend_theme_defaults\n"
        )
        subprocess.run(["bash", "-c", program], env=env, check=True, timeout=5)
        if fifo:
            assert destination.is_fifo()
        elif initial is not None:
            assert destination.read_text() == initial
            assert destination.stat().st_mode & 0o777 == 0o640
            assert destination.is_symlink() == link
        elif legacy or custom:
            assert not destination.exists()
        else:
            assert 'ui_theme = "inherit"' in destination.read_text()
            assert not list(destination.parent.glob(".config.toml.*"))
        print(name, "PASS")

    run("fresh")
    run("explicit-light", '# usercomment\ntheme = "light"\n[unknown]\nkeep=42\n')
    run("explicit-dark", '# spacing preserved\ntheme="dark"\n')
    run("named-override", 'ui_theme="paper" # handchosen\n')
    run("existing-empty", "")
    run("legacy-theme", legacy=True)
    run("explicit-path", custom=True)
    run("existing-symlink", 'theme="dark"\n', link=True)
    run("existing-fifo", fifo=True)
print("Frontend inheritance defaults preserve all user-owned sources: PASS")
