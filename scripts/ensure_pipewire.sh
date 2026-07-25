#!/usr/bin/env bash
# Ensure PipeWire development metadata new enough for jwm-portal is available.
#
# The portal binds the pipewire-rs v1_2_0 API surface, whose generated
# bindings need PipeWire >= 1.2 headers (spa_meta_sync_timeline and friends).
# Distributions shipping older PipeWire (for example Ubuntu 24.04's 1.0.x)
# can still build the portal by letting this script compile a minimal,
# libraries-only PipeWire into a private prefix.
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
#   JWM_PIPEWIRE_VERSION  version to build if needed   (default 1.2.7)
#   JWM_PIPEWIRE_PREFIX   install prefix for the build (default
#                         ~/.cache/jwm/pipewire-prefix)
set -euo pipefail

MIN_VERSION=${JWM_PIPEWIRE_MIN:-1.2.0}
BUILD_VERSION=${JWM_PIPEWIRE_VERSION:-1.2.7}
PREFIX=${JWM_PIPEWIRE_PREFIX:-"$HOME/.cache/jwm/pipewire-prefix"}
ENV_OUTPUT=false
[[ "${1:-}" == "--env" ]] && ENV_OUTPUT=true

log() { echo "ensure_pipewire: $*" >&2; }

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

# 1. A system installation that satisfies the minimum needs nothing else.
if pkg-config --atleast-version="$MIN_VERSION" libpipewire-0.3 2>/dev/null; then
    log "system PipeWire $(pkg-config --modversion libpipewire-0.3) >= $MIN_VERSION; nothing to do"
    exit 0
fi

# 2. A previously built prefix is reused as-is.
if pc_dir=$(prefix_pkgconfig_dir); then
    version=$(PKG_CONFIG_PATH="$pc_dir" pkg-config --modversion libpipewire-0.3)
    if PKG_CONFIG_PATH="$pc_dir" pkg-config --atleast-version="$MIN_VERSION" libpipewire-0.3; then
        log "reusing PipeWire $version already built in $PREFIX"
        emit_env "$pc_dir"
        exit 0
    fi
    log "prefix at $PREFIX holds $version < $MIN_VERSION; rebuilding"
fi

# 3. Build a minimal, libraries-only PipeWire.
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
    https://gitlab.freedesktop.org/pipewire/pipewire.git "$workdir/pipewire" >&2

meson setup "$workdir/build" "$workdir/pipewire" \
    --prefix="$PREFIX" \
    --buildtype=release \
    -Dauto_features=disabled \
    -Dsession-managers=[] \
    -Dspa-plugins=disabled \
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
