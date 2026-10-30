#!/usr/bin/env bash
# Build the default bar without installing it or changing user configuration.
set -euo pipefail
ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
TARGET=${1:-"$ROOT/bars/tauri_leptos_bar/src-tauri/target"}
[[ "$TARGET" == /* ]] || TARGET="$PWD/$TARGET"
bash "$ROOT/scripts/check-webkitgtk-version.sh"
command -v trunk >/dev/null || { echo 'Trunk is required: cargo install trunk --locked' >&2; exit 1; }
(
    cd "$ROOT/bars/tauri_leptos_bar"
    trunk build --release --locked
)
cargo build --locked --release --manifest-path "$ROOT/bars/tauri_leptos_bar/src-tauri/Cargo.toml" \
    --features tauri/custom-protocol --target-dir "$TARGET"
test -x "$TARGET/release/tauri_leptos_bar"
