#!/usr/bin/env bash
# Install only the native applications for the current user, without sudo.
set -euo pipefail
ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
mode=release
case "${1:-}" in
    --debug) mode=debug; shift ;;
    -h|--help)
        printf '%s\n' 'Usage: bash scripts/install-native-apps.sh [--debug]' \
            'Build and install native notes and image viewer for the current user.' \
            'Set text/image defaults only in JWM sessions.'
        exit 0 ;;
esac
[[ $# == 0 ]] || { printf 'Unexpected argument: %s\n' "$1" >&2; exit 2; }
pkg-config --exists cairo pangocairo gio-2.0 || {
    printf '%s\n' 'Cairo/Pango/GLib development files are required (libcairo2-dev libpango1.0-dev libglib2.0-dev).' >&2
    exit 1
}
target=${CARGO_TARGET_DIR:-$ROOT/target}
[[ $target == /* ]] || target="$ROOT/$target"
args=()
[[ $mode != release ]] || args+=(--release)
cargo build --locked --manifest-path "$ROOT/crates/jwm_apps/Cargo.toml" \
    --target-dir "$target/native-apps" "${args[@]}" --bins
data_home=${XDG_DATA_HOME:-$HOME/.local/share}
[[ $data_home == /* ]] || data_home="$HOME/.local/share"
config_home=${XDG_CONFIG_HOME:-$HOME/.config}
[[ $config_home == /* ]] || config_home="$HOME/.config"
bin_dir="$HOME/.local/bin"
mkdir -p -- "$bin_dir" "$data_home/applications"
for binary in jwm-notes jwm-image-viewer; do
    # Atomic replacement also works while an older application is running.
    temporary=$(mktemp "$bin_dir/.$binary.XXXXXXXX")
    install -m755 "$target/native-apps/$mode/$binary" "$temporary"
    mv -f -- "$temporary" "$bin_dir/$binary"
done
# Absolute Exec/TryExec paths work even when the desktop session's PATH lacks
# ~/.local/bin. Quote desktop Exec arguments using the desktop-entry grammar.
python3 - "$ROOT/crates/jwm_apps/dist" "$data_home/applications" "$bin_dir" <<'PY'
from pathlib import Path
import re
import sys

source, destination, binary_dir = map(Path, sys.argv[1:])
def quote(value):
    # Older xdg-utils parses the first word without removing quotes. Keep
    # simple paths bare so both GIO and that implementation find the binary.
    if re.fullmatch(r'[A-Za-z0-9_./+@-]+', value):
        return value
    escaped = ''.join('\\' + char if char in '\\"`$' else char for char in value)
    # The desktop file's string decoder runs before Exec argument parsing.
    return ('"' + escaped + '"').replace('\\', '\\\\').replace('%', '%%')

def key_string(value):
    return value.replace('\\', '\\\\').replace('\n', '\\n').replace('\r', '\\r').replace('\t', '\\t')

for desktop, binary in (("org.jwm.Notes.desktop", "jwm-notes"),
                        ("org.jwm.ImageViewer.desktop", "jwm-image-viewer")):
    path = str(binary_dir / binary)
    lines = (source / desktop).read_text().splitlines()
    lines = ["Exec=" + quote(path) + " -- %F" if line.startswith("Exec=") else
             "TryExec=" + key_string(path) if line.startswith("TryExec=") else line for line in lines]
    (destination / desktop).write_text("\n".join(lines) + "\n")
PY
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$data_home/applications"
fi
python3 "$ROOT/scripts/set-jwm-defaults.py" "$config_home/jwm-mimeapps.list" \
    "$data_home/applications/org.jwm.Notes.desktop" "$data_home/applications/org.jwm.ImageViewer.desktop"
printf 'Installed jwm-notes and jwm-image-viewer to %s; JWM text/image defaults configured.\n' "$bin_dir"
