#!/usr/bin/env bash
# Install the launcher only; never installs or executes project dependencies.
set -euo pipefail
ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
[[ $# == 0 ]] || { echo 'Usage: bash scripts/install-tsx-preview.sh' >&2; exit 2; }
# Match Notes' user-owned installation boundary; never repair permissions or
# follow a pre-existing .local/bin symlink on the user's behalf.
bin=$(python3 - <<'CHECK'
import os
from pathlib import Path
import stat
home = Path(os.environ.get("HOME", ""))
if not home.is_absolute():
    raise SystemExit("HOME must be an absolute path; nothing installed.")
home = home.resolve(strict=True)
for directory in (home, home / ".local", home / ".local/bin"):
    if not directory.exists() and not directory.is_symlink():
        directory.mkdir(mode=0o755)
    info = directory.lstat()
    if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o022:
        raise SystemExit(f"Refusing unsafe installation directory: {directory}; no permissions changed.")
launcher = home / ".local/bin/jwm-tsx-preview"
if launcher.exists() or launcher.is_symlink():
    info = launcher.lstat()
    if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o022:
        raise SystemExit(f"Refusing unsafe existing launcher: {launcher}; nothing replaced.")
print(home / ".local/bin")
CHECK
)
temporary=$(mktemp "$bin/.jwm-tsx-preview.XXXXXXXX")
trap 'rm -f -- "$temporary"' EXIT
install -m755 "$ROOT/scripts/jwm-tsx-preview.mjs" "$temporary"
mv -fT -- "$temporary" "$bin/jwm-tsx-preview"
printf 'Installed %s/jwm-tsx-preview from this checkout; no project code was run.\n' "$bin"
# Metadata only: do not execute Node, the launcher, or project files here.
python3 - <<'CHECK'
from pathlib import Path
try:
    node = Path("/usr/bin/node").resolve(strict=True)
    trusted = node.is_relative_to(Path("/usr/bin")) and node.is_file()
    for path in (node, *node.parents):
        info = path.stat()
        trusted = trusted and info.st_uid == 0 and not info.st_mode & 0o022
    trusted = trusted and bool(node.stat().st_mode & 0o111)
except OSError:
    trusted = False
if trusted:
    print("System Node path meets Notes metadata checks. Notes must still match this launcher revision and validate your selected project.")
else:
    print("Launcher installed, but Notes preflight is NOT ready: trusted /usr/bin/node is missing or unsafe. Install a system Node package yourself; PATH/nvm is not a fallback.")
print("Use this command from the SAME checkout/revision used to build Notes after upgrades. Installation grants no project trust and does not start preview.")
CHECK
