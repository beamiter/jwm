#!/usr/bin/env bash
# bootstrap_deps.sh - Install every system dependency and the Rust toolchain
# needed to build JWM (multi-backend X11 + Wayland window manager/compositor)
# from a clean machine.
#
# Idempotent: safe to re-run. Targets Debian/Ubuntu (apt). On other distros it
# prints the required library groups so you can map them to your package manager.
#
# Usage:
#   bash scripts/bootstrap_deps.sh                 # full bootstrap
#   bash scripts/bootstrap_deps.sh --no-rust       # skip Rust toolchain install
#   bash scripts/bootstrap_deps.sh --no-apt        # skip apt packages
#   JWM_WITH_PORTAL=1 bash scripts/bootstrap_deps.sh   # also add PipeWire portal deps
#   JWM_CN_MIRROR=1 bash scripts/bootstrap_deps.sh     # use rsproxy.cn (China) for rustup + cargo
#
# After this succeeds, build/install JWM with:
#   bash scripts/install_jwm_scripts.sh
set -euo pipefail

# ------------------------------------------------------------------
# Coloured logging
# ------------------------------------------------------------------
RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; CYAN='\033[0;36m'; NC='\033[0m'
info()  { echo -e "${CYAN}[INFO]${NC}  $*"; }
ok()    { echo -e "${GREEN}[OK]${NC}    $*"; }
warn()  { echo -e "${YELLOW}[WARN]${NC}  $*"; }
err()   { echo -e "${RED}[ERROR]${NC} $*" >&2; }

# ------------------------------------------------------------------
# Options
# ------------------------------------------------------------------
DO_APT=true
DO_RUST=true
WITH_PORTAL="${JWM_WITH_PORTAL:-0}"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --no-apt)  DO_APT=false; shift ;;
        --no-rust) DO_RUST=false; shift ;;
        --with-portal) WITH_PORTAL=1; shift ;;
        --cn) JWM_CN_MIRROR=1; export JWM_CN_MIRROR; shift ;;
        -h|--help)
            grep '^#' "$0" | sed 's/^# \{0,1\}//'
            exit 0 ;;
        *) err "unknown option: $1"; exit 1 ;;
    esac
done

# ------------------------------------------------------------------
# sudo helper (works whether or not we are already root)
# ------------------------------------------------------------------
SUDO=""
if [[ "$(id -u)" -ne 0 ]]; then
    if command -v sudo >/dev/null 2>&1; then
        SUDO="sudo"
    else
        err "not root and sudo not available; re-run as root or install sudo"
        exit 1
    fi
fi

# ------------------------------------------------------------------
# System build dependencies (Debian/Ubuntu package names)
# ------------------------------------------------------------------
# Toolchain / bindgen: build-essential + clang/libclang power the C shims and
# every bindgen-based crate (xkbcommon, drm-ffi, gbm, libseat, smithay's
# use_system_lib path). pkg-config locates the native libraries below.
APT_TOOLCHAIN=(
    build-essential pkg-config cmake ninja-build
    clang libclang-dev llvm-dev
    git curl ca-certificates python3
)

# X11 side (the `x11` and `x11rb`/`xcb` crates). Feature set in Cargo.toml pulls
# glx, xlib_xcb, xcursor, xfixes, xinput, xrandr, xrender, dpms, xf86vmode.
APT_X11=(
    libx11-dev libx11-xcb-dev libxext-dev
    libxrandr-dev libxrender-dev libxfixes-dev libxcursor-dev
    libxi-dev libxxf86vm-dev libxinerama-dev libxft-dev
    libxkbcommon-dev libxkbcommon-x11-dev
    # XCB core + the extensions the `xcb` crate enables
    libxcb1-dev libxcb-composite0-dev libxcb-shape0-dev libxcb-randr0-dev
    libxcb-damage0-dev libxcb-present-dev libxcb-xfixes0-dev
    libxcb-render0-dev libxcb-render-util0-dev libxcb-shm0-dev
    libxcb-util-dev libxcb-keysyms1-dev libxcb-icccm4-dev
    libxcb-image0-dev libxcb-cursor-dev
)

# Wayland / DRM / GBM / EGL-GLES compositor stack (smithay udev backend).
APT_WAYLAND=(
    libwayland-dev wayland-protocols
    libdrm-dev libgbm-dev
    libegl1-mesa-dev libgles2-mesa-dev libgl1-mesa-dev
    libinput-dev libseat-dev libudev-dev
    libpixman-1-dev
    libvulkan-dev
    # libseat's built-in backend links libsystemd (logind session support).
    libsystemd-dev
)

# Audio (alsa crate) + D-Bus (portal / session integration) + fonts.
APT_MISC=(
    libasound2-dev
    libdbus-1-dev
    libfontconfig1-dev libfreetype-dev
    libssl-dev
)

# Status-bar rendering stack. The selectable bars (xcb_bar, gtk_bar, egui_bar,
# …) draw through glib/cairo/pango; cairo needs its XCB surface backend.
APT_BAR=(
    libglib2.0-dev libcairo2-dev libpango1.0-dev
    libgdk-pixbuf-2.0-dev libharfbuzz-dev
)

# Runtime helpers useful for nested testing / running a session.
APT_RUNTIME=(
    xwayland xserver-xephyr
)

# Optional: xdg-desktop-portal ScreenCast backend (PipeWire).
APT_PORTAL=(
    libpipewire-0.3-dev libspa-0.2-dev
)

install_apt() {
    local pkgs=("$@")
    info "apt-get install: ${#pkgs[@]} packages"
    $SUDO apt-get install -y --no-install-recommends "${pkgs[@]}"
}

if [[ "$DO_APT" == true ]]; then
    if ! command -v apt-get >/dev/null 2>&1; then
        warn "apt-get not found — this bootstrap targets Debian/Ubuntu."
        warn "Install the equivalents of these groups with your package manager:"
        warn "  toolchain: build-essential pkg-config cmake clang libclang"
        warn "  x11:       libx11 libxcb + extensions libxkbcommon"
        warn "  wayland:   libwayland libdrm libgbm libEGL libGLES libinput libseat libudev pixman"
        warn "  misc:      libasound2 libdbus-1 fontconfig freetype openssl"
        exit 1
    fi

    info "Updating apt package index..."
    $SUDO apt-get update -qq

    ALL_PKGS=(
        "${APT_TOOLCHAIN[@]}"
        "${APT_X11[@]}"
        "${APT_WAYLAND[@]}"
        "${APT_MISC[@]}"
        "${APT_BAR[@]}"
        "${APT_RUNTIME[@]}"
    )
    if [[ "$WITH_PORTAL" == "1" ]]; then
        ALL_PKGS+=("${APT_PORTAL[@]}")
    fi

    install_apt "${ALL_PKGS[@]}"
    ok "System packages installed."
else
    info "Skipping apt packages (--no-apt)."
fi

# ------------------------------------------------------------------
# Optional China mirror (rsproxy.cn). Speeds up the rustup toolchain download
# AND the crates.io / git dependency fetches during the build, which are
# otherwise very slow from mainland China. Enable with JWM_CN_MIRROR=1.
#   - rustup gets RUSTUP_DIST_SERVER / RUSTUP_UPDATE_ROOT below.
#   - cargo gets a global ~/.cargo/config.toml source replacement (versions and
#     checksums still come from Cargo.lock; only the download host changes).
# The repo's own .cargo/config.toml is intentionally left untouched so the tree
# stays host-independent.
# ------------------------------------------------------------------
CN_MIRROR="${JWM_CN_MIRROR:-0}"
RUSTUP_INIT_URL="https://sh.rustup.rs"

setup_cn_cargo_mirror() {
    local cfg="$HOME/.cargo/config.toml"
    mkdir -p "$HOME/.cargo"
    if [[ -f "$cfg" ]] && ! grep -q 'rsproxy-sparse' "$cfg"; then
        cp "$cfg" "$cfg.pre-jwm.$(date +%s)"
        warn "Existing ~/.cargo/config.toml backed up before adding mirror."
    fi
    cat > "$cfg" <<'EOF'
# China mirror (rsproxy.cn) for fast crates.io + git fetches.
# Source replacement keeps the exact versions/checksums from Cargo.lock;
# it only changes where the bytes are downloaded from.
[source.crates-io]
replace-with = "rsproxy-sparse"

[source.rsproxy-sparse]
registry = "sparse+https://rsproxy.cn/index/"

[net]
git-fetch-with-cli = true
EOF
    ok "Configured cargo mirror at $cfg"
}

if [[ "$CN_MIRROR" == "1" ]]; then
    info "China mirror enabled (rsproxy.cn)."
    export RUSTUP_DIST_SERVER="https://rsproxy.cn"
    export RUSTUP_UPDATE_ROOT="https://rsproxy.cn/rustup"
    RUSTUP_INIT_URL="https://rsproxy.cn/rustup-init.sh"
    setup_cn_cargo_mirror
fi

# ------------------------------------------------------------------
# Rust toolchain (rustup). rust-toolchain.toml pins the stable channel with
# clippy + rustfmt; rustup installs that toolchain (with those components)
# automatically the first time cargo runs inside the repo — so we do not pass
# --component here (rustup-init wants a comma-separated list, and the pinned
# file is the source of truth anyway).
# ------------------------------------------------------------------
if [[ "$DO_RUST" == true ]]; then
    if command -v cargo >/dev/null 2>&1; then
        ok "cargo already present: $(cargo --version)"
    elif [[ -f "$HOME/.cargo/env" ]]; then
        # shellcheck source=/dev/null
        source "$HOME/.cargo/env"
        ok "cargo found via ~/.cargo/env: $(cargo --version)"
    else
        info "Installing Rust toolchain via rustup..."
        curl --proto '=https' --tlsv1.2 -sSf "$RUSTUP_INIT_URL" \
            | sh -s -- -y --profile minimal
        # shellcheck source=/dev/null
        source "$HOME/.cargo/env"
        ok "Rust installed: $(cargo --version)"
    fi
else
    info "Skipping Rust toolchain (--no-rust)."
fi

echo ""
ok "Bootstrap complete. Next:"
echo "    bash scripts/install_jwm_scripts.sh          # build + install jwm and a status bar"
echo "    cargo build --locked --release               # or just build in-tree"
