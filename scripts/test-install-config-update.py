#!/usr/bin/env python3
"""Exercise real updater/bar synchronization using an isolated fake binary."""
from pathlib import Path
import os
import subprocess
import tempfile

source = (Path(__file__).resolve().parent / "install-jwm.sh").read_text()

def function(name):
    start = source.index(name + "() {")
    end = source.index("\n# ============================================================", start)
    return source[start:end]

with tempfile.TemporaryDirectory(prefix="jwm-update-test-") as directory:
    root = Path(directory)
    binary = root / "jwm"
    binary.write_text('''#!/bin/sh
set -eu
printf '%s\\n%s\\n' "$XDG_CONFIG_HOME" "$*" > "$UPDATE_LOG"
for name in ${UPDATE_FILES:-}; do
    path="$XDG_CONFIG_HOME/jwm/config_$name.toml"
    mkdir -p "$XDG_CONFIG_HOME/jwm"
    if [ "${UPDATE_MODE:-normal}" = normal ]; then
        printf '[status_bar]\\nname="tao_glow_bar"\\n' > "$path.tmp"
        mv -f -- "$path.tmp" "$path"
        printf 'Generated configuration: %s\\n' "$path"
    else
        printf '# concurrent user edit\\n[status_bar]\\nname="custom_bar"\\n' > "$path"
        printf 'Keeping configuration (fields unchanged): %s\\n' "$path"
    fi
done
[ "${UPDATE_MODE:-normal}" != failed-concurrent ] || exit 7
''')
    binary.chmod(0o755)
    regenerate = function("regenerate_config").replace(
        'local jwm_bin="/usr/local/bin/jwm"', f'local jwm_bin="{binary}"')
    start = source.index("update_toml_status_bar_name() ")
    end = source.index("sync_selected_bar_config() {")
    edit = source[start:end]
    setup = '''set -euo pipefail
info() { :; }; ok() { :; }; warn() { :; }; err() { exit 1; };
jwm_config_dir() { printf '%s/jwm\\n' "$TEST_CONFIG_HOME"; }
id() { printf '1000\\n'; }
UPDATED_CONFIG_PATHS=()
'''
    cases = [
        # name, files initially present, files generated/replaced, explicit -b
        ("unchanged", ("x11", "wayland"), (), False, "normal"),
        ("new-x11", ("wayland",), ("x11",), False, "normal"),
        ("replaced-x11", ("x11", "wayland"), ("x11",), False, "normal"),
        ("both-new", (), ("x11", "wayland"), False, "normal"),
        ("explicit-bar", ("x11", "wayland"), (), True, "normal"),
        ("concurrent-edit", ("x11", "wayland"), ("x11",), False, "concurrent"),
        ("failed-concurrent-edit", ("x11", "wayland"), ("x11",), False, "failed-concurrent"),
    ]
    for name, existing, updated, explicit, mode in cases:
        generated = updated if mode == "normal" else ()
        config_home = root / name / "config space"
        config_dir = config_home / "jwm"
        config_dir.mkdir(parents=True)
        for family in existing:
            (config_dir / f"config_{family}.toml").write_text(
                '[status_bar]\nname="custom_bar"\n')
        log = root / name / "log"
        env = dict(os.environ, UPDATE_LOG=str(log), TEST_CONFIG_HOME=str(config_home),
                   UPDATE_FILES=" ".join(updated), UPDATE_MODE=mode)
        chosen = "tao_pixels_bar" if explicit else "tao_glow_bar"
        program = (setup + edit + regenerate + function("sync_selected_bar_config")
                   + f'\nJWM_BAR_SET_BY_ARGS={str(explicit).lower()}\n'
                   + f'regenerate_config\nsync_selected_bar_config {chosen}\n'
                   + f'[[ ${{#UPDATED_CONFIG_PATHS[@]}} == {len(generated)} ]]\n')
        subprocess.run(["bash", "-c", program], env=env, check=True)
        assert log.read_text().splitlines() == [str(config_home), "--update-config"]
        for family in ("x11", "wayland"):
            expected = chosen if explicit or family in generated else "custom_bar"
            content = (config_dir / f"config_{family}.toml").read_text()
            assert f'"{expected}"' in content, (name, family, content)
        print(name, "PASS")
print("installer config updater: PASS")
