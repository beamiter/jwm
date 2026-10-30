#!/bin/bash
# install-jwm.sh - 安装 JWM、Eva，并按需安装 status bar
set -euo pipefail

# ============================================================
# 确保 cargo 可用（sudo 环境下 PATH 可能不包含 cargo）
# ============================================================
if ! command -v cargo &>/dev/null; then
    for candidate in "$HOME/.cargo/env" "/home/${SUDO_USER:-}/.cargo/env"; do
        if [[ -f "$candidate" ]]; then
            # shellcheck source=/dev/null
            source "$candidate"
            break
        fi
    done
fi
if ! command -v cargo &>/dev/null; then
    echo "[ERROR] cargo 未找到（sudo 环境下 PATH 可能不包含 cargo），请先安装 Rust 工具链或不使用 sudo 运行此脚本" >&2
    exit 1
fi

# ============================================================
# 颜色输出
# ============================================================
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
NC='\033[0m'

info()  { echo -e "${CYAN}[INFO]${NC}  $*"; }
ok()    { echo -e "${GREEN}[OK]${NC}    $*"; }
warn()  { echo -e "${YELLOW}[WARN]${NC}  $*"; }
err()   { echo -e "${RED}[ERROR]${NC} $*"; }

# ============================================================
# 项目根目录（脚本所在目录的上一级）
# ============================================================
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
BARS_DIR="$PROJECT_ROOT/bars"
JWM_TARGET_DIR="${CARGO_TARGET_DIR:-$PROJECT_ROOT/target}"
if [[ "$JWM_TARGET_DIR" != /* ]]; then
    JWM_TARGET_DIR="$PROJECT_ROOT/$JWM_TARGET_DIR"
fi

# Recover only a missing generated XCB source, preserving other build failures.
build_workspace() (
    local log status
    local -a build_status
    log=$(mktemp "${TMPDIR:-/tmp}/jwm-build.XXXXXXXX")
    trap 'rm -f -- "$log"' EXIT
    # Capture both statuses immediately, including when the pipeline fails.
    if cargo build --locked --target-dir "$JWM_TARGET_DIR" "${CARGO_BUILD_MODE_ARGS[@]}" "${CARGO_JOBS_ARGS[@]}" "$@" 2>&1 | tee "$log"; then
        build_status=("${PIPESTATUS[@]}")
    else
        build_status=("${PIPESTATUS[@]}")
    fi
    status=${build_status[0]}
    ((status != 0)) || status=${build_status[1]}
    # The diagnostic contains literal Rust crate-name backticks.
    # shellcheck disable=SC2016
    if ((build_status[0] != 0 && build_status[1] == 0)) &&
        grep -Eq "couldn't read .*[/]build/xcb-[^/]+/out/[^/]+[.]rs.*(No such file|os error 2)" "$log" &&
        grep -Fq 'could not compile `xcb`' "$log"; then
        warn "XCB 生成文件缺失，清理当前模式的 XCB 缓存并重试一次..."
        local clean_profile=(--release)
        [[ "$BUILD_MODE" == release ]] || clean_profile=(--profile dev)
        cargo clean -p xcb "${clean_profile[@]}" --target-dir "$JWM_TARGET_DIR" || return $?
        cargo build --locked --target-dir "$JWM_TARGET_DIR" "${CARGO_BUILD_MODE_ARGS[@]}" "${CARGO_JOBS_ARGS[@]}" "$@"
        return $?
    fi
    return "$status"
)

# ============================================================
# 所有支持的 bar（monorepo 内 bars/ 目录下的独立实现）
# ============================================================
ALL_BARS=(
    dioxus_bar
    egui_bar
    gtk_bar
    iced_bar
    relm_bar
    tao_glow_bar
    tao_pixels_bar
    tao_softbuffer_bar
    tao_wgpu_bar
    tauri_leptos_bar
    tauri_react_bar
    tauri_solid_bar
    tauri_svelte_bar
    tauri_vue_bar
    tauri_yew_bar
    winit_glow_bar
    winit_pixels_bar
    winit_softbuffer_bar
    winit_wgpu_bar
    xcb_glow_bar
    xcb_wgpu_bar
    x11rb_glow_bar
    x11rb_wgpu_bar
    x11rb_bar
    xcb_bar
    xilem_bar
    gpui_bar
    gpui_component_bar
)

# ============================================================
# 默认值
# ============================================================
BUILD_MODE="release"
JWM_BAR_NAME="${JWM_BAR_NAME:-tauri_leptos_bar}"
JWM_BAR_SET_BY_ARGS=false
UPDATED_CONFIG_PATHS=()
SKIP_BAR=false
SKIP_JWM=false
SKIP_EVA=false
REGEN_CONFIG=false
JOBS=""

# ============================================================
# 帮助信息
# ============================================================
usage() {
    local status=${1:-0}
    cat <<EOF
用法: $(basename "$0") [选项]

选项:
  -m, --mode <debug|release>  构建模式（默认: release）
  -b, --bar <bar_name>        指定要构建的 bar（默认: $JWM_BAR_NAME；bars/ 目录内的 crate，monorepo 自带全部源码）。
                              jwm 启动时通过 config_x11.toml/config_wayland.toml 的 status_bar.name 选择 bar，
                              不再使用 cargo feature。
  -l, --list-bars             列出所有可用的 bar
  -j, --jobs <N>              并行编译任务数（传给 cargo）
  --gen-config                比较新旧默认模板字段，仅字段变化时备份并更新配置
  --skip-bar                  跳过 bar 安装
  --skip-jwm                  跳过 jwm 和 Eva 编译安装（仅处理 bar）
  --skip-eva                  跳过 Eva 语音助手编译安装
  单独安装/更新内置壁纸（无需编译）: bash scripts/install-wallpapers.sh
  -h, --help                  显示此帮助信息

说明:
  - 推荐日用会话是 Wayland（jwm-wayland.desktop / wayland-udev）；X11 会话条目仍会安装，作为兼容面。
  - 官方默认 status bar 是 tauri_leptos_bar；可通过 JWM_BAR_NAME 或 -b 选择其它 bar，-b 优先。
  - 所有 bar 的源码都在本仓库 bars/ 目录下，随 jwm 一起版本化，无需克隆外部仓库。
  - 真正会被安装的 bar 只有 JWM_BAR_NAME（即 -b 的第一个参数、环境变量或官方默认值）。
  - 原生 Rust bar 使用 cargo install --path ... 安装到 cargo bin 目录（通常是 ~/.cargo/bin）。
    dioxus_bar 使用 dx；Tauri bar 会同时构建前端与 src-tauri 后端，再把可执行文件安装到同一目录。
  - jwm / jwm-tool / jwm-support / jwm-remote / jwm-player 通过 cargo build 构建，并安装到 /usr/local/bin，不会安装到 cargo bin。
  - 完整安装默认同时构建 Eva，沿用 --mode、--jobs 和 CARGO_TARGET_DIR，安装到登录用户的 ~/.local/bin。
    保留已有 eva.toml；图形用户会话可用时启用并重启，否则仅安装文件并提示后续启用命令。
  - 音乐和视频播放运行时需要 mpv；缺少它不影响 JWM 的构建与安装。
  - 通过用户配置目录中的 jwm-mimeapps.list 设置 JWM 会话的默认音视频播放器；Ubuntu/GNOME 的默认设置不变。
  - jwm 通过 ~/.config/jwm/config_x11.toml 和 config_wayland.toml 的 status_bar.name
    在运行时选择 bar，切换 bar 不需要重编 jwm。
  - 完整安装还将内置静态壁纸复制到 ~/.config/jwm/wallpapers/（支持 XDG_CONFIG_HOME）；
    重装只更新原样保留的受管文件，不覆盖用户修改或同名自定义文件。
  - 显式指定 -b/--bar 后同步两份配置的 bar；默认安装仅同步新建或字段变化后重建的配置，保留原配置的 bar。
    每次完整安装比较已记录和新版默认模板的字段；仅默认值变化保留现有配置。
    首次接管已有配置时保留原文件并记录模板；字段增删时才备份并更新。
    缺失的配置会自动生成；手动 jwm --gen-config 可主动强制重置。

示例:
  $(basename "$0")                           # 安装 jwm + 官方默认 bar（tauri_leptos_bar）
  $(basename "$0") --gen-config              # 同上，并按字段变化更新配置
  $(basename "$0") -m debug                  # debug 模式编译安装
  $(basename "$0") -b xcb_bar                # 安装示例 bar（非官方默认）
  $(basename "$0") -b xcb_bar --skip-jwm     # 仅安装该 bar
  $(basename "$0") --gen-config --skip-bar   # 检查并更新配置，不安装 bar
EOF
    exit "$status"
}

list_bars() {
    info "可用的 status bar:"
    for bar in "${ALL_BARS[@]}"; do
        echo "  - $bar"
    done
    exit 0
}

is_valid_bar() {
    local candidate="$1"
    local bar

    for bar in "${ALL_BARS[@]}"; do
        if [[ "$bar" == "$candidate" ]]; then
            return 0
        fi
    done

    return 1
}

add_selected_bars() {
    local raw_value="$1"
    local candidate=""
    local parsed_bars=()

    IFS=',' read -r -a parsed_bars <<< "$raw_value"
    for candidate in "${parsed_bars[@]}"; do
        candidate="${candidate//[[:space:]]/}"

        if [[ -z "$candidate" ]]; then
            continue
        fi

        if ! is_valid_bar "$candidate"; then
            err "未知的 bar: $candidate"
            info "使用 --list-bars 查看所有可用的 bar"
            exit 1
        fi

        if [[ "$JWM_BAR_SET_BY_ARGS" == false ]]; then
            JWM_BAR_NAME="$candidate"
            JWM_BAR_SET_BY_ARGS=true
        else
            warn "忽略额外的 bar: $candidate（每次只构建一个 bar）"
        fi
    done
}

require_option_value() {
    if [[ $# -lt 2 || -z $2 || $2 == -* ]]; then
        err "$1 需要参数值"
        exit 2
    fi
}

# ============================================================
# 参数解析
# ============================================================
while [[ $# -gt 0 ]]; do
    case "$1" in
        -m|--mode)
            require_option_value "$@"
            BUILD_MODE="$2"
            if [[ "$BUILD_MODE" != "debug" && "$BUILD_MODE" != "release" ]]; then
                err "构建模式必须是 debug 或 release"
                exit 1
            fi
            shift 2
            ;;
        -b|--bar)
            require_option_value "$@"
            add_selected_bars "$2"
            shift 2
            ;;
        -l|--list-bars)
            list_bars
            ;;
        -j|--jobs)
            require_option_value "$@"
            JOBS="$2"
            if [[ ! $JOBS =~ ^[1-9][0-9]*$ ]]; then
                err "并行编译任务数必须是正整数: $JOBS"
                exit 2
            fi
            shift 2
            ;;
        --gen-config)
            REGEN_CONFIG=true
            shift
            ;;
        --skip-bar)
            SKIP_BAR=true
            shift
            ;;
        --skip-jwm)
            SKIP_JWM=true
            shift
            ;;
        --skip-eva)
            SKIP_EVA=true
            shift
            ;;
        -h|--help)
            usage
            ;;
        *)
            err "未知选项: $1"
            usage 2
            ;;
    esac
done

if [[ "$SKIP_BAR" == false ]] && ! is_valid_bar "$JWM_BAR_NAME"; then
    err "未知的 bar: $JWM_BAR_NAME"
    exit 2
fi

# ============================================================
# 构建参数
# ============================================================
if [[ "$BUILD_MODE" == "release" ]]; then
    CARGO_INSTALL_MODE_ARGS=()
    CARGO_BUILD_MODE_ARGS=(--release)
else
    CARGO_INSTALL_MODE_ARGS=(--debug)
    CARGO_BUILD_MODE_ARGS=()
fi

CARGO_JOBS_ARGS=()
if [[ -n "$JOBS" ]]; then
    CARGO_JOBS_ARGS=(-j "$JOBS")
fi

# ============================================================
# cargo install 目标目录
# ============================================================
cargo_install_root() {
    if [[ -n "${CARGO_HOME:-}" ]]; then
        echo "$CARGO_HOME"
        return
    fi

    local target_home="${HOME}"
    if [[ "$(id -u)" -eq 0 && -n "${SUDO_USER:-}" ]]; then
        local sudo_home
        sudo_home="$(getent passwd "$SUDO_USER" | cut -d: -f6)"
        if [[ -n "$sudo_home" ]]; then
            target_home="$sudo_home"
        else
            target_home="/home/$SUDO_USER"
        fi
    fi

    echo "$target_home/.cargo"
}

cargo_bin_dir() {
    echo "$(cargo_install_root)/bin"
}

target_user_home() {
    local target_home="${HOME}"
    if [[ "$(id -u)" -eq 0 && -n "${SUDO_USER:-}" ]]; then
        local sudo_home
        sudo_home="$(getent passwd "$SUDO_USER" | cut -d: -f6)"
        if [[ -n "$sudo_home" ]]; then
            target_home="$sudo_home"
        else
            target_home="/home/$SUDO_USER"
        fi
    fi

    echo "$target_home"
}

target_user_data_home() {
    if [[ ${XDG_DATA_HOME:-} == /* ]]; then
        printf '%s\n' "$XDG_DATA_HOME"
    else
        printf '%s/.local/share\n' "$(target_user_home)"
    fi
}

jwm_config_dir() {
    # XDG only permits absolute paths; a relative override must not install
    # resources outside the target user's config home.
    if [[ ${XDG_CONFIG_HOME:-} == /* ]]; then
        echo "$XDG_CONFIG_HOME/jwm"
    else
        echo "$(target_user_home)/.config/jwm"
    fi
}

install_jwm_media_defaults() {
    local config_dir destination
    config_dir=$(jwm_config_dir)
    destination="${config_dir%/jwm}/jwm-mimeapps.list"
    local -a command=(python3 - "$destination" "$PROJECT_ROOT/bridge/dist")
    if [[ "$(id -u)" -eq 0 && -n "${SUDO_USER:-}" ]]; then
        command=(sudo -u "$SUDO_USER" "${command[@]}")
    fi

    # Desktop-specific defaults take precedence over mimeapps.list only when
    # XDG_CURRENT_DESKTOP includes jwm. Never rewrite the shared user defaults.
    "${command[@]}" <<'PY'
import configparser
import io
import os
from pathlib import Path
import shutil
import stat
import sys
import tempfile

destination = Path(os.path.normpath(sys.argv[1]))
desktop_dir = Path(sys.argv[2])

def read_config(text):
    config = configparser.ConfigParser(interpolation=None, delimiters=("=",))
    config.optionxform = str
    config.read_string(text)
    return config

if destination.is_symlink():
    raise SystemExit(f"Refusing to replace symlink: {destination}")
previous = destination.read_bytes() if destination.exists() else None
config = read_config(previous.decode("utf-8") if previous is not None else "")
section = "Default Applications"
if not config.has_section(section):
    config.add_section(section)
changed = previous is None
for desktop_id in ("jwm-player.desktop", "jwm-video.desktop"):
    desktop = read_config((desktop_dir / desktop_id).read_text(encoding="utf-8"))
    for mime in desktop["Desktop Entry"]["MimeType"].split(";"):
        if not mime:
            continue
        value = desktop_id + ";"
        if config.get(section, mime, fallback=None) != value:
            config.set(section, mime, value)
            changed = True

if changed:
    output = io.StringIO()
    config.write(output, space_around_delimiters=False)
    destination.parent.mkdir(parents=True, exist_ok=True)
    mode = stat.S_IMODE(destination.stat().st_mode) if previous is not None else 0o644
    fd, temporary = tempfile.mkstemp(prefix=".jwm-mimeapps-", dir=destination.parent)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            handle.write(output.getvalue())
        os.chmod(temporary, mode)
        if previous is not None:
            shutil.copy2(destination, str(destination) + ".bak")
        os.replace(temporary, destination)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)
PY
    ok "已设置 JWM 会话的默认音视频播放器（其他桌面不变）: $destination"
}

install_bundled_wallpapers() {
    local destination
    destination="$(jwm_config_dir)/wallpapers"
    info "安装内置静态壁纸到 $destination ..."

    # sudo builds must not produce root-owned files in the login user's home.
    # Pass a resolved path instead of relying on sudo's HOME/XDG inheritance.
    if [[ "$(id -u)" -eq 0 && -n "${SUDO_USER:-}" ]]; then
        sudo -u "$SUDO_USER" bash "$PROJECT_ROOT/scripts/install-wallpapers.sh" --dest "$destination"
    else
        bash "$PROJECT_ROOT/scripts/install-wallpapers.sh" --dest "$destination"
    fi
}

ensure_cargo_bin_dir() {
    mkdir -p "$(cargo_bin_dir)"
}

remove_jwm_cargo_bins() {
    local bin_dir
    bin_dir="$(cargo_bin_dir)"
    local binary

    # JWM 不再安装到 cargo bin；清理历史版本遗留的 JWM 二进制。
    for binary in jwm jwm-tool jwm-support jwm-remote jwm-player; do
        if [[ -e "$bin_dir/$binary" ]]; then
            info "清理旧的 cargo bin/$binary ..."
            rm -f -- "$bin_dir/$binary"
        fi
    done
}

install_system_binary() {
    local src="$1"
    local dest_dir="$2"

    if [[ ! -x "$src" ]]; then
        err "二进制不存在或不可执行: $src"
        exit 1
    fi

    sudo install -m755 "$src" "$dest_dir/"
}

# ============================================================
# 安装 bar 到 cargo bin
# ============================================================
build_tauri_bar() {
    local bar="$1"
    local bar_dir="$BARS_DIR/$bar"
    local tauri_dir="$bar_dir/src-tauri"
    local target_dir="$tauri_dir/target"
    local profile="release"
    local tauri_args=(build --no-bundle)
    local build_env=(env "CARGO_TARGET_DIR=$target_dir")

    if [[ ! -f "$tauri_dir/Cargo.toml" || ! -f "$tauri_dir/tauri.conf.json" ]]; then
        err "$bar 缺少完整的 src-tauri 构建入口"
        exit 1
    fi
    if ! command -v pkg-config &>/dev/null || ! pkg-config --exists webkit2gtk-4.1; then
        err "缺少 Tauri 2 Linux 系统依赖（WebKitGTK 4.1）"
        err "Debian/Ubuntu 安装：bash $PROJECT_ROOT/scripts/bootstrap_deps.sh --with-tauri --no-rust"
        exit 1
    fi

    if [[ "$BUILD_MODE" == "debug" ]]; then
        profile="debug"
        tauri_args+=(--debug)
    fi
    if [[ -n "$JOBS" ]]; then
        build_env+=("CARGO_BUILD_JOBS=$JOBS")
    fi

    if [[ "$bar" == tauri_leptos_bar ]]; then
        tauri_args+=(-- --locked)
    fi

    info "构建 $bar 前端与 Tauri 后端（$BUILD_MODE 模式）..."
    if [[ -f "$bar_dir/package.json" ]]; then
        if ! command -v pnpm &>/dev/null; then
            err "pnpm 未找到；构建 $bar 需要 Node.js + pnpm"
            err "安装后重试：corepack enable pnpm"
            exit 1
        fi

        (
            cd "$bar_dir"
            if [[ -f pnpm-lock.yaml ]]; then
                pnpm install --frozen-lockfile
            else
                # 无锁文件的示例不应在安装 JWM 时改写源码树。
                pnpm install --lockfile=false
            fi
            "${build_env[@]}" pnpm exec tauri "${tauri_args[@]}"
        )
    else
        if [[ ! -f "$bar_dir/Cargo.toml" || ! -f "$bar_dir/Trunk.toml" ]]; then
            err "$bar 缺少 Rust/Wasm 前端的 Cargo.toml 或 Trunk.toml"
            exit 1
        fi
        if ! command -v trunk &>/dev/null; then
            err "trunk 未找到；构建 $bar 需要 Trunk"
            err "安装后重试：cargo install trunk --version 0.22.1 --locked"
            exit 1
        fi
        local tauri_cli_version
        if ! tauri_cli_version="$(cargo tauri --version 2>/dev/null)"; then
            err "cargo-tauri 未找到；构建 $bar 需要 Tauri CLI 2"
            err "安装后重试：cargo install tauri-cli --version 2.12.4 --locked"
            exit 1
        fi
        if [[ ! "$tauri_cli_version" =~ (^|[[:space:]])2\. ]]; then
            err "检测到不兼容的 Tauri CLI：$tauri_cli_version（需要 major 2）"
            err "升级后重试：cargo install tauri-cli --version 2.12.4 --locked --force"
            exit 1
        fi
        if command -v rustup &>/dev/null \
            && ! rustup target list --installed | grep -qx 'wasm32-unknown-unknown'; then
            err "缺少 wasm32-unknown-unknown Rust target"
            err "安装后重试：rustup target add wasm32-unknown-unknown"
            exit 1
        fi

        (
            cd "$bar_dir"
            "${build_env[@]}" cargo tauri "${tauri_args[@]}"
        )
    fi

    local output="$target_dir/$profile/$bar"
    if [[ ! -x "$output" ]]; then
        err "$bar 构建产物不存在或不可执行: $output"
        exit 1
    fi

    install -m755 "$output" "$(cargo_bin_dir)/$bar"
    ok "$bar 安装完成: $(cargo_bin_dir)/$bar"
}

build_bar() {
    local bar="$1"
    local bar_dir="$BARS_DIR/$bar"

    ensure_cargo_bin_dir

    if [[ -f "$bar_dir/src-tauri/Cargo.toml" ]]; then
        build_tauri_bar "$bar"
        return
    fi

    if [[ ! -f "$bar_dir/Cargo.toml" ]]; then
        err "$bar_dir/Cargo.toml 不存在，无法编译"
        exit 1
    fi

    # Dioxus desktop 的最终可执行文件由 dx 输出，而不是 cargo build/install 的默认 target 路径。
    if [[ "$bar" == "dioxus_bar" ]]; then
        local dioxus_output="$bar_dir/target/dx/dioxus_bar/release/linux/app/dioxus_bar"

        if ! command -v dx &>/dev/null; then
            err "dx 未找到；请先安装 Dioxus CLI（cargo install dioxus-cli）"
            exit 1
        fi

        info "构建 dioxus_bar（dx build --release）..."
        (
            cd "$bar_dir"
            dx build --release
        )

        if [[ ! -x "$dioxus_output" ]]; then
            err "dioxus_bar 构建产物不存在或不可执行: $dioxus_output"
            exit 1
        fi

        install -m755 "$dioxus_output" "$(cargo_bin_dir)/dioxus_bar"
        ok "dioxus_bar 安装完成: $(cargo_bin_dir)/dioxus_bar"
        return
    fi

    info "安装 $bar（$BUILD_MODE 模式）..."
    cargo install --locked --target-dir "$JWM_TARGET_DIR/bar-install/$bar" --path "$bar_dir" --force "${CARGO_INSTALL_MODE_ARGS[@]}" "${CARGO_JOBS_ARGS[@]}" --root "$(cargo_install_root)"

    ok "$bar 安装完成: $(cargo_bin_dir)"
}

# Build the shared host independently of the selected frontend. It is installed
# with the session so opting into desktop_shell never depends on a Tao bar.
build_and_install_shell() {
    info "构建共享三侧栏宿主 jwm-shell（$BUILD_MODE 模式）..."
    cargo build --locked --manifest-path "$PROJECT_ROOT/crates/jwm_shell/Cargo.toml" \
        --target-dir "$JWM_TARGET_DIR/shell-host" "${CARGO_BUILD_MODE_ARGS[@]}" "${CARGO_JOBS_ARGS[@]}"
    local profile=release
    [[ "$BUILD_MODE" == release ]] || profile=debug
    install_system_binary "$JWM_TARGET_DIR/shell-host/$profile/jwm-shell" /usr/local/bin
}

# Build with the main installer's profile/cache, then delegate user files and
# service handling to the standalone installer. A sudo invocation must not
# install Eva or its private PipeWire runtime in root's home.
build_and_install_eva() {
    local user_home config_dir user_uid variable setting pipewire_environment
    user_home=$(target_user_home)
    config_dir=$(jwm_config_dir)
    local -a user_command=()
    if [[ "$(id -u)" -eq 0 && -n "${SUDO_USER:-}" ]]; then
        user_uid=$(id -u "$SUDO_USER")
        user_command=(sudo -u "$SUDO_USER" env "HOME=$user_home"
            "XDG_RUNTIME_DIR=/run/user/$user_uid"
            "DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/$user_uid/bus"
            "XDG_CONFIG_HOME=${config_dir%/jwm}")
        for variable in DISPLAY WAYLAND_DISPLAY XDG_SESSION_TYPE XDG_CURRENT_DESKTOP \
            PKG_CONFIG_PATH JWM_PIPEWIRE_PREFIX; do
            if [[ -v $variable ]]; then user_command+=("$variable=${!variable}"); fi
        done
    fi

    info "编译 Eva 语音助手（$BUILD_MODE 模式）..."
    if ! command -v pkg-config >/dev/null || ! pkg-config --exists libpulse; then
        err "Eva 需要 PulseAudio 开发头文件（Debian/Ubuntu: libpulse-dev）；可运行依赖引导脚本，或用 --skip-eva 跳过"
        return 1
    fi
    pipewire_environment=$("${user_command[@]}" bash "$PROJECT_ROOT/scripts/ensure_pipewire.sh" --env)
    (
        while IFS= read -r setting; do
            case "$setting" in
                PKG_CONFIG_PATH=*|JWM_PIPEWIRE_PREFIX=*) export "${setting?}" ;;
                '') ;;
                *) err "Eva 收到非预期的 PipeWire 构建环境变量"; exit 1 ;;
            esac
        done <<< "$pipewire_environment"
        cd "$PROJECT_ROOT"
        build_workspace -p jwm-eva --bin jwm-eva
    )

    local -a arguments=(--binary "$JWM_TARGET_DIR/$BUILD_MODE/jwm-eva"
        --prefix "${JWM_EVA_PREFIX:-$user_home/.local}"
        --config-dir "$config_dir" --unit-dir "${config_dir%/jwm}/systemd/user")
    local enable_service=true
    if ! "${user_command[@]}" systemctl --user show --property=Version >/dev/null 2>&1 ||
        ! "${user_command[@]}" systemctl --user is-active --quiet graphical-session.target; then
        arguments+=(--no-enable)
        enable_service=false
    fi
    "${user_command[@]}" bash "$PROJECT_ROOT/scripts/install-eva.sh" "${arguments[@]}"
    if [[ $enable_service == false ]]; then
        warn "Eva 文件已安装，当前没有可用的图形用户会话；登录图形会话后运行："
        info "  systemctl --user daemon-reload"
        info "  systemctl --user enable --now jwm-eva.service"
    fi
}

# ============================================================
# 同步选中的 bar 到用户配置
# ============================================================
update_toml_status_bar_name() (
    local path="$1"
    local bar="$2"
    local tmp

    # A shell line editor cannot distinguish TOML multiline values. Validate
    # semantic preservation before publishing; no parser means no write.
    if [[ ! -f "$path" || -L "$path" ]]; then
        err "配置必须是普通文件；符号链接请手动设置 status_bar.name: $path"
        return 1
    fi
    if ! command -v python3 >/dev/null 2>&1; then
        err "安全更新配置需要 Python 3.11+，或 Python 3 + tomli；请手动设置 status_bar.name"
        return 1
    fi
    tmp="$(mktemp "${path}.tmp.XXXXXX")" || return 1
    trap 'rm -f -- "$tmp"' EXIT
    awk -v bar="$bar" '
        BEGIN {
            in_status_bar = 0
            saw_status_bar = 0
            wrote_name = 0
        }

        /^\[status_bar\][[:space:]]*$/ {
            print
            in_status_bar = 1
            saw_status_bar = 1
            wrote_name = 0
            next
        }

        /^\[/ && in_status_bar {
            if (!wrote_name) {
                print "name = \"" bar "\""
            }
            in_status_bar = 0
            print
            next
        }

        in_status_bar && /^[[:space:]]*name[[:space:]]*=/ {
            print "name = \"" bar "\""
            wrote_name = 1
            next
        }

        { print }

        END {
            if (in_status_bar && !wrote_name) {
                print "name = \"" bar "\""
            } else if (!saw_status_bar) {
                print ""
                print "[status_bar]"
                print "name = \"" bar "\""
            }
        }
    ' "$path" > "$tmp" || return 1
    if ! python3 - "$path" "$tmp" "$bar" <<'PYTOML'
import sys
try:
    try:
        import tomllib
    except ImportError:
        import tomli as tomllib
    with open(sys.argv[1], "rb") as source:
        before = tomllib.load(source)
    with open(sys.argv[2], "rb") as source:
        after = tomllib.load(source)
    section = after.get("status_bar")
    if not isinstance(section, dict) or section.get("name") != sys.argv[3]:
        raise ValueError("status_bar.name was not updated as requested")
    for value in (before, after):
        section = value.get("status_bar")
        if isinstance(section, dict):
            section.pop("name", None)
            if not section:
                value.pop("status_bar")
    if before != after:
        raise ValueError("line edit would alter unrelated TOML values")
except (ImportError, OSError, ValueError) as error:
    print("配置未改动；需要 Python 3.11+ 或 tomli，且修改必须仅影响 status_bar.name: "
          + str(error), file=sys.stderr)
    sys.exit(1)
PYTOML
    then
        return 1
    fi
    chmod --reference="$path" "$tmp" || return 1
    mv -- "$tmp" "$path"
)

sync_selected_bar_config() {
    local bar="$1"
    local config_dir
    local path

    if [[ -z "$bar" ]]; then
        return
    fi

    config_dir="$(jwm_config_dir)"
    mkdir -p "$config_dir"

    if [[ ! -f "$config_dir/config_x11.toml" || ! -f "$config_dir/config_wayland.toml" ]]; then
        info "config_x11.toml 或 config_wayland.toml 不存在，先生成默认配置..."
        regenerate_config
    fi

    for path in "$config_dir/config_x11.toml" "$config_dir/config_wayland.toml"; do
        if [[ ! -f "$path" ]]; then
            err "配置文件不存在，无法同步 bar: $path"
            err "请确认 jwm --gen-config 可用，或手动生成该配置文件后重试"
            exit 1
        fi

        # Existing configurations with unchanged fields keep their custom bar.
        # Fresh/replaced templates must select the bar this install actually built.
        if [[ "$JWM_BAR_SET_BY_ARGS" != true ]]; then
            local updated_path selected=false
            for updated_path in "${UPDATED_CONFIG_PATHS[@]}"; do
                [[ "$updated_path" != "$path" ]] || selected=true
            done
            [[ "$selected" == true ]] || continue
        fi
        update_toml_status_bar_name "$path" "$bar"
        ok "已同步 bar 到配置: $path -> $bar"
    done
}

# ============================================================
# 构建并安装 jwm-bridge 和 jwm-player
#
# bridge/ 是 workspace 成员，产物落在根 target/；主 cargo build 只构建
# 根 package，所以这里显式 -p jwm-bridge。
# 它是 org.freedesktop.Notifications 与 MPRIS 的唯一提供者：少了它，
# 飞书等应用发出的通知在会话总线上无人接收，桌面完全不弹通知。
# 因此把它和 jwm 一起安装，并注册 D-Bus 激活文件由总线按需拉起。
# ============================================================
build_and_install_bridge() {
    local bridge_dir="$PROJECT_ROOT/bridge"

    if [[ ! -f "$bridge_dir/Cargo.toml" ]]; then
        err "$bridge_dir/Cargo.toml 不存在，无法编译 jwm-bridge"
        exit 1
    fi

    info "编译 jwm-bridge 和 jwm-player（$BUILD_MODE 模式）..."
    (cd "$PROJECT_ROOT" && build_workspace -p jwm-bridge --bins)

    local bridge_target="$JWM_TARGET_DIR"
    if [[ "$BUILD_MODE" == "release" ]]; then
        bridge_target="$bridge_target/release"
    else
        bridge_target="$bridge_target/debug"
    fi

    info "同步 jwm-bridge 和 jwm-player 到 /usr/local/bin ..."
    install_system_binary "$bridge_target/jwm-bridge" /usr/local/bin
    install_system_binary "$bridge_target/jwm-player" /usr/local/bin

    # Register Open With choices, then set defaults for JWM sessions only.
    local desktop
    for desktop in jwm-player.desktop jwm-video.desktop; do
        sudo install -Dm644 "$bridge_dir/dist/$desktop" "/usr/local/share/applications/$desktop"
    done
    if command -v update-desktop-database >/dev/null 2>&1; then
        sudo update-desktop-database /usr/local/share/applications || \
            warn "播放器已安装，桌面应用缓存更新失败；可稍后运行 update-desktop-database"
    fi
    install_jwm_media_defaults
    if ! command -v mpv >/dev/null 2>&1; then
        warn "音乐/视频播放需要 mpv；Debian/Ubuntu: sudo apt-get install mpv（JWM 已正常安装）"
    fi

    # D-Bus 按需激活：应用第一次调用 Notify 时总线才启动 bridge，
    # 不需要写进任何 autostart，也能在 jwm 重启后继续工作。
    local service_destination
    service_destination="$(target_user_data_home)/dbus-1/services/org.freedesktop.Notifications.service"
    if [[ "$(id -u)" -eq 0 && -n "${SUDO_USER:-}" ]]; then
        sudo -u "$SUDO_USER" install -Dm644 \
            "$bridge_dir/dist/org.freedesktop.Notifications.service" "$service_destination"
    else
        install -Dm644 "$bridge_dir/dist/org.freedesktop.Notifications.service" "$service_destination"
    fi

    ok "jwm-bridge 安装完成，并已注册 org.freedesktop.Notifications 激活文件"
    # Replacing the file does not retire an old D-Bus owner. In particular a
    # wedged bridge makes Feishu wait 25s on GetCapabilities before drawing UI.
    # The helper verifies the exact owner/uid/executable and uses a pidfd;
    # healthy current builds and other notification servers are untouched.
    local reload_status=0
    if [[ "$(id -u)" -eq 0 && -n "${SUDO_USER:-}" ]]; then
        sudo -u "$SUDO_USER" timeout 8s /usr/local/bin/jwm-bridge reload || reload_status=$?
    else
        timeout 8s /usr/local/bin/jwm-bridge reload || reload_status=$?
    fi
    if ((reload_status != 0)); then
        warn "通知桥文件已更新，但运行中旧进程未能自动重载；请以桌面用户运行 /usr/local/bin/jwm-bridge reload"
    fi
}

# ============================================================
# 构建 JWM，并将 desktop 依赖的二进制安装到 /usr/local/bin
# ============================================================
build_and_install_jwm() {
    info "安装 jwm（$BUILD_MODE 模式）..."

    if [[ -n "$JWM_BAR_NAME" ]]; then
        info "当前 bar: $JWM_BAR_NAME（bar 已安装到 cargo bin；jwm 通过用户配置的 status_bar.name 选择 bar）"
    fi

    cd "$PROJECT_ROOT"

    # JWM 不使用 cargo install，避免把 jwm/jwm-tool/jwm-support/jwm-remote 写入 cargo bin。
    build_workspace
    build_and_install_shell

    local target_dir="$JWM_TARGET_DIR"
    if [[ "$BUILD_MODE" == "release" ]]; then
        target_dir="$target_dir/release"
    else
        target_dir="$target_dir/debug"
    fi

    info "同步 jwm, jwm-tool, jwm-support, jwm-remote 到 /usr/local/bin ..."
    install_system_binary "$target_dir/jwm" /usr/local/bin
    install_system_binary "$target_dir/jwm-tool" /usr/local/bin
    install_system_binary "$target_dir/jwm-support" /usr/local/bin
    install_system_binary "$target_dir/jwm-remote" /usr/local/bin
    install_system_binary "$PROJECT_ROOT/scripts/jwm-toggle-sidebar-mode" /usr/local/bin
    sudo install -Dm644 "$PROJECT_ROOT/packaging/69-jwm-touchpad-gestures.rules" \
        /usr/lib/udev/rules.d/69-jwm-touchpad-gestures.rules
    if command -v udevadm >/dev/null 2>&1; then
        sudo udevadm control --reload-rules || warn "udev 规则将在下次设备发现时生效"
        sudo udevadm trigger --action=change --subsystem-match=input --property-match=ID_INPUT_TOUCHPAD=1 --sysname-match='event*' || \
            warn "触摸板访问规则将在下次设备发现时生效"
    fi

    # Shared data is kept outside the executable and installed for both build modes.
    sudo install -Dm644 "$PROJECT_ROOT/assets/astronomy/earth-nasa-october.jpg" \
        /usr/local/share/jwm/astronomy/earth-nasa-october.jpg
    # Preserve January for users with an explicit texture path.
    sudo install -Dm644 "$PROJECT_ROOT/assets/astronomy/earth-nasa-january.jpg" \
        /usr/local/share/jwm/astronomy/earth-nasa-january.jpg
    sudo install -Dm644 "$PROJECT_ROOT/assets/astronomy/moon-nasa-lroc.jpg" \
        /usr/local/share/jwm/astronomy/moon-nasa-lroc.jpg
    sudo install -Dm644 "$PROJECT_ROOT/assets/astronomy/moon-nasa-lroc-8k.jpg" \
        /usr/local/share/jwm/astronomy/moon-nasa-lroc-8k.jpg
    sudo install -Dm644 "$PROJECT_ROOT/assets/astronomy/earth-nasa-clouds.jpg" \
        /usr/local/share/jwm/astronomy/earth-nasa-clouds.jpg
    sudo install -Dm644 "$PROJECT_ROOT/assets/astronomy/milkyway-nasa-gaia.jpg" \
        /usr/local/share/jwm/astronomy/milkyway-nasa-gaia.jpg
    sudo install -Dm644 "$PROJECT_ROOT/assets/astronomy/galaxy-nasa-4k.png" \
        /usr/local/share/jwm/astronomy/galaxy-nasa-4k.png
    sudo install -Dm644 "$PROJECT_ROOT/assets/astronomy/deep-sky-atlas.jpg" \
        /usr/local/share/jwm/astronomy/deep-sky-atlas.jpg
    sudo install -Dm644 "$PROJECT_ROOT/assets/astronomy/deep-sky-catalog.json" \
        /usr/local/share/jwm/astronomy/deep-sky-catalog.json
    sudo install -Dm644 "$PROJECT_ROOT/assets/astronomy/mercury-nasa-messenger.jpg" \
        /usr/local/share/jwm/astronomy/mercury-nasa-messenger.jpg
    sudo install -Dm644 "$PROJECT_ROOT/assets/astronomy/mars-nasa-viking.jpg" \
        /usr/local/share/jwm/astronomy/mars-nasa-viking.jpg
    sudo install -Dm644 "$PROJECT_ROOT/assets/astronomy/jupiter-nasa-cassini.jpg" \
        /usr/local/share/jwm/astronomy/jupiter-nasa-cassini.jpg
    sudo install -Dm644 "$PROJECT_ROOT/assets/astronomy/README.md" \
        /usr/local/share/jwm/astronomy/README.md

    ok "jwm, jwm-tool, jwm-support, jwm-remote 安装完成: /usr/local/bin（未安装到 cargo bin）"

    build_and_install_bridge

    info "安装 desktop 文件 ..."
    [[ -f jwm-x11rb.desktop ]]         && sudo install -m644 jwm-x11rb.desktop         /usr/share/xsessions/
    [[ -f jwm-x11rb-debug.desktop ]]   && sudo install -m644 jwm-x11rb-debug.desktop   /usr/share/xsessions/
    [[ -f jwm-xcb.desktop ]]           && sudo install -m644 jwm-xcb.desktop           /usr/share/xsessions/
    [[ -f jwm-xcb-debug.desktop ]]     && sudo install -m644 jwm-xcb-debug.desktop     /usr/share/xsessions/
    [[ -f jwm-wayland.desktop ]]       && sudo install -m644 jwm-wayland.desktop       /usr/share/wayland-sessions/
    [[ -f jwm-wayland-debug.desktop ]] && sudo install -m644 jwm-wayland-debug.desktop /usr/share/wayland-sessions/
    ok "desktop 文件安装完成"
}

# ============================================================
# 更新 JWM 配置文件（按字段比较；字段相同时保留自定义值）
# ============================================================
regenerate_config() {
    info "比较 JWM 新旧默认模板字段并按需更新配置..."
    local jwm_bin="/usr/local/bin/jwm"
    if [[ ! -x "$jwm_bin" ]]; then
        jwm_bin="$JWM_TARGET_DIR/$BUILD_MODE/jwm"
    fi
    local config_home config_dir path before after index=0
    local -a previous_revisions=()
    config_dir="$(jwm_config_dir)"
    config_home="$(dirname -- "$config_dir")"
    for path in "$config_dir/config_x11.toml" "$config_dir/config_wayland.toml"; do
        before=""
        if [[ -f "$path" && ! -L "$path" ]]; then
            before="$(stat -c '%d:%i:%s:%y:%z' -- "$path")"
        fi
        previous_revisions+=("$before")
    done
    local -a command=(env "XDG_CONFIG_HOME=$config_home" "$jwm_bin" --update-config)
    if [[ "$(id -u)" -eq 0 && -n "${SUDO_USER:-}" ]]; then
        command=(sudo -u "$SUDO_USER" "${command[@]}")
    fi
    local output generated_ok=false
    output="$(mktemp "${TMPDIR:-/tmp}/jwm-config-update.XXXXXXXX")"
    if "${command[@]}" > "$output"; then
        generated_ok=true
    fi
    # Keep the binary's output intact, including partial success before an error.
    if ! cat -- "$output"; then
        rm -f -- "$output"
        return 1
    fi
    if [[ "$generated_ok" == true ]]; then
        ok "配置字段检查完成"
    else
        warn "配置字段检查失败，原配置未主动重置，请手动运行: $jwm_bin --update-config"
    fi
    for path in "$config_dir/config_x11.toml" "$config_dir/config_wayland.toml"; do
        if [[ -f "$path" && ! -L "$path" ]]; then
            after="$(stat -c '%d:%i:%s:%y:%z' -- "$path")"
            # A revision change alone may belong to a concurrent user edit.
            # Only synchronize templates this invocation explicitly published.
            if [[ "$after" != "${previous_revisions[index]}" ]] \
                && grep -Fxq -- "Generated configuration: $path" "$output"; then
                UPDATED_CONFIG_PATHS+=("$path")
            fi
        fi
        index=$((index + 1))
    done
    rm -f -- "$output"
}

# ============================================================
# 显示 jwm-tool 帮助
# ============================================================
show_jwm_tool_help() {
    echo ""
    info "jwm-tool - JWM 管理工具（单二进制多子命令）"
    echo "Usage: jwm-tool <COMMAND>"
    echo "Commands:"
    echo "  daemon          启动守护进程"
    echo "  restart         向守护进程发送命令"
    echo "  stop"
    echo "  start"
    echo "  quit"
    echo "  status"
    echo "  rebuild         编译并重启 JWM"
    echo "  daemon-check    守护进程存活检查"
    echo "  daemon-restart"
    echo "  debug           调试信息"
    echo "  help            Print this message or the help of the given subcommand(s)"
    echo "Options:"
    echo "  -h, --help     Print help"
    echo "  -V, --version  Print version"
}

# ============================================================
# 主流程
# ============================================================
echo ""
info "========================================="
info " JWM 安装脚本"
info " 构建模式: $BUILD_MODE"
info " JWM Bar (installed only): $JWM_BAR_NAME"
info " 重新生成配置: $REGEN_CONFIG"
info "========================================="
echo ""

# 1. 安装 JWM_BAR_NAME 对应的 bar 到 cargo bin（源码就在 bars/ 目录）
if [[ "$SKIP_BAR" == false && -n "$JWM_BAR_NAME" ]]; then
    build_bar "$JWM_BAR_NAME"
fi

# 2. 处理 jwm
if [[ "$SKIP_JWM" == false ]]; then
    build_and_install_jwm
    install_bundled_wallpapers
    if [[ "$SKIP_EVA" == false ]]; then build_and_install_eva; fi
    # Only migrate the legacy cargo installation after every replacement and
    # its session/bridge integration succeeded. A skipped or failed install
    # must leave the user's previously working commands available.
    remove_jwm_cargo_bins
    show_jwm_tool_help
fi

# 3. 完整安装及 --gen-config 都按字段变化更新配置
if [[ "$REGEN_CONFIG" == true ]]; then
    regenerate_config
elif [[ "$SKIP_JWM" == false ]]; then
    regenerate_config
fi

# 4. 同步选中的 bar 到用户配置。放在 --gen-config 之后，避免重新生成配置覆盖选择。
if [[ "$SKIP_BAR" == false && -n "$JWM_BAR_NAME" ]]; then
    sync_selected_bar_config "$JWM_BAR_NAME"
fi

echo ""
ok "全部完成！"
echo ""
info "日用推荐: 在显示管理器里选「JWM」Wayland 会话（jwm-wayland.desktop）。"
info "启动前跑: jwm --backend wayland-udev --doctor"
info "官方 bar: ${JWM_BAR_NAME:-tauri_leptos_bar}（见 bars/README.md）"
info "真机验收清单: docs/hardware-validation.md"
