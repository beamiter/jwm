#!/usr/bin/env bash
# Compile the real six build scripts with only tauri_build stubbed, then
# exercise their requirement check against real pkg-config package metadata.
set -euo pipefail
ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
TEST_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/jwm-tauri-version.XXXXXXXX")
trap 'rm -rf -- "$TEST_ROOT"' EXIT
real_pkg_config=$(command -v pkg-config)
mkdir -p "$TEST_ROOT/pkgconfig"
fail() { printf 'test-tauri-webkit-version: FAIL: %s\n' "$*" >&2; exit 1; }

check_version() {
    local binary=$1 version=$2 expected=$3 target_os=${4:-linux} status=0
    rm -f "$TEST_ROOT/built" "$TEST_ROOT/pkgconfig/webkit2gtk-4.1.pc"
    if [[ $version != missing ]]; then
        cat > "$TEST_ROOT/pkgconfig/webkit2gtk-4.1.pc" <<PC
Name: WebKitGTK test SDK
Description: Version-only build prerequisite fixture
Version: $version
PC
    fi
    CARGO_CFG_TARGET_OS="$target_os" PKG_CONFIG="$real_pkg_config" \
        PKG_CONFIG_PATH='' PKG_CONFIG_LIBDIR="$TEST_ROOT/pkgconfig" PKG_CONFIG_SYSROOT_DIR='' \
        TAURI_BUILD_MARKER="$TEST_ROOT/built" "$binary" > "$TEST_ROOT/output.log" 2>&1 || status=$?
    if [[ $expected == pass ]]; then
        [[ $status == 0 && -f $TEST_ROOT/built ]] || fail "$(basename "$binary"): $version/$target_os was rejected"
    else
        [[ $status != 0 && ! -e $TEST_ROOT/built ]] || fail "$(basename "$binary"): $version reached the Tauri build"
        grep -Fq 'WebKitGTK >= 2.52.6' "$TEST_ROOT/output.log" || fail 'required version omitted from diagnostic'
        if [[ $version != missing ]]; then
            grep -Fq "detected: $version" "$TEST_ROOT/output.log" || fail 'detected version omitted from diagnostic'
        fi
    fi
}

for host in leptos react solid svelte vue yew; do
    manifest="$ROOT/bars/tauri_${host}_bar/src-tauri"
    cat > "$TEST_ROOT/build.rs" <<RS
mod tauri_build {
    pub fn build() {
        std::fs::write(std::env::var_os("TAURI_BUILD_MARKER").unwrap(), "built").unwrap();
    }
}
include!(r#"$manifest/build.rs"#);
RS
    binary="$TEST_ROOT/$host"
    CARGO_MANIFEST_DIR="$manifest" rustc --edition 2024 "$TEST_ROOT/build.rs" -o "$binary"
    for version in 2.50.4 2.52.3 2.52.5 missing; do
        check_version "$binary" "$version" fail
    done
    for version in 2.52.6 2.54.2 3.0.0; do
        check_version "$binary" "$version" pass
    done
    check_version "$binary" missing pass windows
    check_version "$binary" missing pass macos
done
# The standalone Leptos helper must reject an old SDK before invoking either
# frontend or backend build tools.
mkdir -p "$TEST_ROOT/bin"
for tool in trunk cargo; do
    cat > "$TEST_ROOT/bin/$tool" <<'STUB'
#!/usr/bin/env bash
touch "$TAURI_BUILD_MARKER"
exit 42
STUB
    chmod +x "$TEST_ROOT/bin/$tool"
done
cat > "$TEST_ROOT/pkgconfig/webkit2gtk-4.1.pc" <<'PC'
Name: WebKitGTK test SDK
Description: Version-only build prerequisite fixture
Version: 2.52.5
PC
status=0
PATH="$TEST_ROOT/bin:$PATH" PKG_CONFIG="$real_pkg_config" \
    PKG_CONFIG_PATH='' PKG_CONFIG_LIBDIR="$TEST_ROOT/pkgconfig" PKG_CONFIG_SYSROOT_DIR='' \
    TAURI_BUILD_MARKER="$TEST_ROOT/standalone-built" bash "$ROOT/scripts/build-leptos-bar.sh" \
    > "$TEST_ROOT/standalone.log" 2>&1 || status=$?
[[ $status == 1 && ! -e $TEST_ROOT/standalone-built ]] || fail 'standalone helper built with an old SDK'
grep -Fq 'detected: 2.52.5' "$TEST_ROOT/standalone.log" || fail 'standalone helper omitted version diagnostic'
printf 'test-tauri-webkit-version: PASS (six hosts, minimum boundary, newer/missing SDKs, non-Linux targets)\n'
