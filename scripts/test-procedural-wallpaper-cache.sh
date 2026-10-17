#!/usr/bin/env bash
# No X server, sockets, workspace dependency resolution, or downloaded crates.
# GLOW_SOURCE_DIR may select a local glow 0.18.0 source; otherwise use Cargo's cache.
set -euo pipefail
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
skip() {
    echo "SKIP: $*" >&2
    if [[ ${JWM_REQUIRE_HEADLESS_GL:-0} == 1 ]]; then exit 1; fi
    exit 0
}
rustc=${RUSTC:-rustc}
command -v "$rustc" >/dev/null || skip "rustc unavailable"
[[ $(uname -s) == Linux ]] || skip "this surfaceless EGL harness requires Linux"
# Rust links the runtime soname, so a distribution development symlink is not needed.
ldconfig_output=$(ldconfig -p 2>/dev/null) || skip "cannot locate the EGL runtime"
[[ $ldconfig_output == *libEGL.so.1* ]] || skip "libEGL.so.1 unavailable"
glow=${GLOW_SOURCE_DIR:-}
if [[ -z $glow ]]; then
    shopt -s nullglob
    candidates=("${CARGO_HOME:-$HOME/.cargo}"/registry/src/*/glow-0.18.0)
    ((${#candidates[@]})) || skip "glow 0.18.0 source unavailable; set GLOW_SOURCE_DIR or fetch Cargo dependencies"
    glow=${candidates[0]}
fi
[[ -f $glow/src/lib.rs ]] || skip "GLOW_SOURCE_DIR does not contain glow sources"
work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT
# The exact production module is included by path: no extraction or copied implementation.
"$rustc" --edition=2021 --crate-type=rlib --crate-name glow "$glow/src/lib.rs" -o "$work/libglow.rlib"
"$rustc" --edition=2024 --test "$root/scripts/test-procedural-wallpaper-cache.rs" \
    --extern "glow=$work/libglow.rlib" -C link-arg=-l:libEGL.so.1 -o "$work/tests"
EGL_PLATFORM=surfaceless LIBGL_ALWAYS_SOFTWARE=1 MESA_SHADER_CACHE_DISABLE=true \
    "$work/tests" --nocapture --test-threads=1
