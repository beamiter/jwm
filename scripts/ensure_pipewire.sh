#!/usr/bin/env bash
# Ensure PipeWire development metadata new enough for jwm-portal is available.
#
# The portal binds the pipewire-rs v1_2_0 API surface, whose generated
# bindings need PipeWire >= 1.2 headers (spa_meta_sync_timeline and friends).
# Distributions shipping older PipeWire (for example Ubuntu 24.04's 1.0.x)
# can still build the portal by letting this script compile a private
# development prefix with the core SPA plugins used by the client library.
#
# Usage:
#   bash scripts/ensure_pipewire.sh          # human output
#   bash scripts/ensure_pipewire.sh --env    # print KEY=VALUE lines for the
#                                            # build environment (empty when
#                                            # the system installation is new
#                                            # enough); suitable for
#                                            # `>> "$GITHUB_ENV"` in CI.
#
# Environment:
#   JWM_PIPEWIRE_MIN      minimum acceptable version   (default 1.2.0)
#   JWM_PIPEWIRE_VERSION  version to build if needed   (default 1.6.10)
#   JWM_PIPEWIRE_PREFIX   install prefix for the build (default
#                         ~/.cache/jwm/pipewire-prefix)
set -euo pipefail

MIN_VERSION=${JWM_PIPEWIRE_MIN:-1.2.0}
BUILD_VERSION=${JWM_PIPEWIRE_VERSION:-1.6.10}
PREFIX=${JWM_PIPEWIRE_PREFIX:-"$HOME/.cache/jwm/pipewire-prefix"}
ENV_OUTPUT=false
[[ "${1:-}" == "--env" ]] && ENV_OUTPUT=true

log() { echo "ensure_pipewire: $*" >&2; }

command -v pkg-config >/dev/null || {
    log "missing pkg-config (install pkg-config before building the portal)"
    exit 1
}
[[ "$PREFIX" == /* && "$PREFIX" != *$'\n'* ]] || {
    log "JWM_PIPEWIRE_PREFIX must be an absolute path without newlines"
    exit 1
}

prefix_pkgconfig_dir() {
    for dir in "$PREFIX/lib/pkgconfig" "$PREFIX/lib64/pkgconfig" \
        "$PREFIX"/lib/*/pkgconfig; do
        if [[ -f "$dir/libpipewire-0.3.pc" ]]; then
            echo "$dir"
            return 0
        fi
    done
    return 1
}

emit_env() {
    local pc_dir="$1"
    if $ENV_OUTPUT; then
        echo "PKG_CONFIG_PATH=$pc_dir${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
        echo "JWM_PIPEWIRE_PREFIX=$PREFIX"
    else
        log "use this environment when building jwm-portal:"
        log "  export PKG_CONFIG_PATH=$pc_dir\${PKG_CONFIG_PATH:+:\$PKG_CONFIG_PATH}"
        log "  export JWM_PIPEWIRE_PREFIX=$PREFIX"
    fi
}

# An explicit private prefix takes priority even when system metadata exists.
# Check SPA too: a runtime package or an incomplete -dev prefix cannot build
# the generated Rust bindings.
use_prefix() {
    local pc_dir version
    pc_dir=$(prefix_pkgconfig_dir) || return 1
    [[ -f "$pc_dir/libspa-0.2.pc" ]] || return 1
    if ! PKG_CONFIG_PATH="$pc_dir" pkg-config --atleast-version="$MIN_VERSION" libpipewire-0.3 ||
       ! PKG_CONFIG_PATH="$pc_dir" pkg-config --exists libspa-0.2; then
        return 1
    fi
    version=$(PKG_CONFIG_PATH="$pc_dir" pkg-config --modversion libpipewire-0.3)
    log "reusing PipeWire $version already built in $PREFIX"
    emit_env "$pc_dir"
}

if [[ -n "${JWM_PIPEWIRE_PREFIX:-}" ]]; then
    if use_prefix; then
        exit 0
    fi
    log "private prefix $PREFIX needs PipeWire >= $MIN_VERSION and SPA development files"
elif pkg-config --atleast-version="$MIN_VERSION" libpipewire-0.3 2>/dev/null &&
     pkg-config --exists libspa-0.2; then
    # A complete system installation needs neither a private build nor rpath.
    log "system PipeWire $(pkg-config --modversion libpipewire-0.3) >= $MIN_VERSION; nothing to do"
    exit 0
elif use_prefix; then
    exit 0
fi

# Build the private development prefix. SPA support must stay enabled for
# PipeWire 1.6's build graph and the client's event loop implementation.
for tool in meson ninja cc git; do
    command -v "$tool" >/dev/null || {
        log "missing build tool '$tool' (install meson, ninja-build, a C toolchain, and git)"
        exit 1
    }
done

workdir=$(mktemp -d "${TMPDIR:-/tmp}/jwm-pipewire-build.XXXXXX")
trap 'rm -rf "$workdir"' EXIT
log "building PipeWire $BUILD_VERSION into $PREFIX"
git clone --depth 1 --branch "$BUILD_VERSION" \
    https://github.com/PipeWire/pipewire.git "$workdir/pipewire" >&2

meson setup "$workdir/build" "$workdir/pipewire" \
    --prefix="$PREFIX" \
    --libdir=lib \
    --buildtype=release \
    -Dauto_features=disabled \
    -Dsession-managers=[] \
    -Dspa-plugins=enabled \
    -Dexamples=disabled \
    -Dtests=disabled \
    -Dman=disabled \
    -Ddocs=disabled >&2
meson compile -C "$workdir/build" >&2
meson install -C "$workdir/build" >&2

pc_dir=$(prefix_pkgconfig_dir) || {
    log "install completed but libpipewire-0.3.pc was not found under $PREFIX"
    exit 1
}
log "built PipeWire $(PKG_CONFIG_PATH="$pc_dir" pkg-config --modversion libpipewire-0.3)"
emit_env "$pc_dir"
