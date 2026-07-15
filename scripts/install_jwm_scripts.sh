#!/bin/bash
# install_jwm_scripts.sh - 安装 JWM，并按需安装 status bar
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
SUBMODULES_DIR="$PROJECT_ROOT/submodules"

GITHUB_USER="beamiter"
GITHUB_BASE="https://github.com/$GITHUB_USER"

# ============================================================
# 所有支持的 bar（与 Cargo.toml features 对应）
# ============================================================
ALL_BARS=(
    dioxus_bar
    egui_bar
    gtk_bar
    iced_bar
    relm_bar
    tao_pixels_bar
    tao_softbuffer_bar
    tao_wgpu_bar
    tauri_leptos_bar
    tauri_react_bar
    tauri_solid_bar
    tauri_svelte_bar
    tauri_vue_bar
    tauri_yew_bar
    winit_pixels_bar
    winit_softbuffer_bar
    winit_wgpu_bar
    xcb_wgpu_bar
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
JWM_BAR_NAME="x11rb_bar"
JWM_BAR_SET_BY_ARGS=false

# CLONE_BARS：仅用于把这些 bar 仓库拉到本地（git clone / pull），
# 不参与编译。实际构建的对象只有 JWM_BAR_NAME 对应的那个 bar。
# 取消注释你希望保留本地副本的 bar 即可。
CLONE_BARS=(
    # dioxus_bar
    # egui_bar
    # gtk_bar
    # iced_bar
    # relm_bar
    # tao_pixels_bar
    # tao_softbuffer_bar
    # tao_wgpu_bar
    # tauri_leptos_bar
    # tauri_react_bar
    # tauri_solid_bar
    # tauri_svelte_bar
    # tauri_vue_bar
    # tauri_yew_bar
    # winit_pixels_bar
    # winit_softbuffer_bar
    # winit_wgpu_bar
    # xcb_wgpu_bar
    # x11rb_wgpu_bar
    # x11rb_bar
    # xcb_bar
    # xilem_bar
    # gpui_bar
    # gpui_component_bar
)
SKIP_BAR=false
SKIP_JWM=false
REGEN_CONFIG=false
JOBS=""

# ============================================================
# 帮助信息
# ============================================================
usage() {
    cat <<EOF
用法: $(basename "$0") [选项]

选项:
  -m, --mode <debug|release>  构建模式（默认: release）
  -b, --bar <bar_name>        指定要构建的 bar，同时把该 bar 加入克隆列表；
                              可重复传入或使用逗号分隔；第一个显式传入的 bar 会被构建，
                              其余仅作为额外的本地克隆目标（不参与构建）。
                              jwm 启动时通过 config_x11.toml/config_wayland.toml 的 status_bar.name 选择 bar，
                              不再使用 cargo feature。
  -l, --list-bars             列出所有可用的 bar
  -j, --jobs <N>              并行编译任务数（传给 cargo）
  --gen-config                安装后重新生成默认配置（备份旧配置为 .toml.backup）
  --skip-bar                  跳过 bar 安装（仍会按 CLONE_BARS 同步代码）
  --skip-jwm                  跳过 jwm 编译安装（仅处理 bar）
  -h, --help                  显示此帮助信息

说明:
  - CLONE_BARS（脚本顶部）只用于把哪些 bar 仓库 git clone/pull 到本地，不会构建。
  - 真正会被安装的 bar 只有 JWM_BAR_NAME（即 -b 的第一个参数，或脚本顶部默认值）。
  - 除 dioxus_bar 外，bar 使用 cargo install --path ... 安装到 cargo bin 目录（通常是 ~/.cargo/bin）。
    dioxus_bar 使用 dx build --release 构建，并把其 Dioxus 产物安装到同一目录。
  - jwm / jwm-tool / jwm-support 只通过 cargo build 构建，并安装到 /usr/local/bin，不会安装到 cargo bin。
  - jwm 通过 ~/.config/jwm/config_x11.toml 和 config_wayland.toml 的 status_bar.name
    在运行时选择 bar，切换 bar 不需要重编 jwm。
  - 安装完成后，脚本会把选中的 bar 同步写入 config_x11.toml 和 config_wayland.toml；
    如果任一配置文件不存在，会先运行 jwm --gen-config 生成默认配置。

示例:
  $(basename "$0")                           # 安装 jwm + 默认 bar，按 CLONE_BARS 同步其它仓库
  $(basename "$0") --gen-config              # 同上，并重新生成默认配置
  $(basename "$0") -m debug                  # debug 模式编译安装
  $(basename "$0") -b xcb_bar                # 安装 xcb_bar
  $(basename "$0") -b xcb_bar,egui_bar       # 安装 xcb_bar；同时把 egui_bar 仓库拉到本地
  $(basename "$0") -b xcb_bar --skip-jwm     # 仅安装 xcb_bar
  $(basename "$0") --gen-config --skip-bar   # 仅重新生成配置，不安装 bar
EOF
    exit 0
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
    local existing=""

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
        fi

        local already_listed=false
        for existing in "${CLONE_BARS[@]}"; do
            if [[ "$existing" == "$candidate" ]]; then
                already_listed=true
                break
            fi
        done

        if [[ "$already_listed" == false ]]; then
            CLONE_BARS+=("$candidate")
        fi
    done
}

# ============================================================
# 参数解析
# ============================================================
while [[ $# -gt 0 ]]; do
    case "$1" in
        -m|--mode)
            BUILD_MODE="$2"
            if [[ "$BUILD_MODE" != "debug" && "$BUILD_MODE" != "release" ]]; then
                err "构建模式必须是 debug 或 release"
                exit 1
            fi
            shift 2
            ;;
        -b|--bar)
            add_selected_bars "$2"
            shift 2
            ;;
        -l|--list-bars)
            list_bars
            ;;
        -j|--jobs)
            JOBS="$2"
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
        -h|--help)
            usage
            ;;
        *)
            err "未知选项: $1"
            usage
            ;;
    esac
done

# ============================================================
# 构建参数
# ============================================================
if [[ "$BUILD_MODE" == "release" ]]; then
    CARGO_INSTALL_MODE_FLAG=""
    CARGO_BUILD_MODE_FLAG="--release"
else
    CARGO_INSTALL_MODE_FLAG="--debug"
    CARGO_BUILD_MODE_FLAG=""
fi

CARGO_JOBS=""
if [[ -n "$JOBS" ]]; then
    CARGO_JOBS="-j $JOBS"
fi

# ============================================================
# 拉取/更新 submodule（bar 代码）
# ============================================================
sync_bar_repo() {
    local bar="$1"
    local repo_url="$GITHUB_BASE/$bar.git"
    local bar_dir="$SUBMODULES_DIR/$bar"

    if [[ -d "$bar_dir/.git" ]]; then
        info "更新 $bar ..."
        git -C "$bar_dir" pull --ff-only || {
            warn "$bar pull 失败，尝试 fetch + reset ..."
            git -C "$bar_dir" fetch origin
            git -C "$bar_dir" reset --hard origin/HEAD
        }
    else
        info "克隆 $bar ..."
        mkdir -p "$SUBMODULES_DIR"
        git clone "$repo_url" "$bar_dir"
    fi
    ok "$bar 代码已就绪: $bar_dir"
}

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

jwm_config_dir() {
    if [[ -n "${XDG_CONFIG_HOME:-}" ]]; then
        echo "$XDG_CONFIG_HOME/jwm"
    else
        echo "$(target_user_home)/.config/jwm"
    fi
}

ensure_cargo_bin_dir() {
    mkdir -p "$(cargo_bin_dir)"
}

remove_jwm_cargo_bins() {
    local bin_dir="$(cargo_bin_dir)"
    local binary

    # JWM 不再安装到 cargo bin；清理历史版本遗留的 JWM 二进制。
    for binary in jwm jwm-tool jwm-support; do
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
build_bar() {
    local bar="$1"
    local bar_dir="$SUBMODULES_DIR/$bar"

    if [[ ! -f "$bar_dir/Cargo.toml" ]]; then
        err "$bar_dir/Cargo.toml 不存在，无法编译"
        exit 1
    fi

    ensure_cargo_bin_dir

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
    # shellcheck disable=SC2086
    cargo install --path "$bar_dir" --force $CARGO_INSTALL_MODE_FLAG $CARGO_JOBS --root "$(cargo_install_root)"

    ok "$bar 安装完成: $(cargo_bin_dir)"
}

# ============================================================
# 同步选中的 bar 到用户配置
# ============================================================
update_toml_status_bar_name() {
    local path="$1"
    local bar="$2"
    local tmp

    tmp="$(mktemp "${path}.tmp.XXXXXX")"
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
                print "show_bar = true"
            }
        }
    ' "$path" > "$tmp"
    mv "$tmp" "$path"
}

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

        update_toml_status_bar_name "$path" "$bar"
        ok "已同步 bar 到配置: $path -> $bar"
    done
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

    # JWM 不使用 cargo install，避免把 jwm/jwm-tool/jwm-support 写入 cargo bin。
    # shellcheck disable=SC2086
    cargo build --locked $CARGO_BUILD_MODE_FLAG $CARGO_JOBS

    local target_dir="$PROJECT_ROOT/target"
    if [[ "$BUILD_MODE" == "release" ]]; then
        target_dir="$target_dir/release"
    else
        target_dir="$target_dir/debug"
    fi

    info "同步 jwm, jwm-tool, jwm-support 到 /usr/local/bin ..."
    install_system_binary "$target_dir/jwm" /usr/local/bin
    install_system_binary "$target_dir/jwm-tool" /usr/local/bin
    install_system_binary "$target_dir/jwm-support" /usr/local/bin

    ok "jwm, jwm-tool, jwm-support 安装完成: /usr/local/bin（未安装到 cargo bin）"

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
# 重新生成 JWM 配置文件（覆盖旧配置，旧配置自动备份）
# ============================================================
regenerate_config() {
    info "重新生成 JWM 配置文件..."
    local jwm_bin="/usr/local/bin/jwm"
    if [[ ! -x "$jwm_bin" ]]; then
        jwm_bin="$PROJECT_ROOT/target/$BUILD_MODE/jwm"
    fi
    if "$jwm_bin" --gen-config; then
        ok "配置文件已重新生成"
    else
        warn "配置文件生成失败，请手动运行: $jwm_bin --gen-config"
    fi
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
    echo "  daemon-check    守护进程检查/重启"
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
# 把要编译的 bar 也加进克隆列表（去重）
if [[ -n "$JWM_BAR_NAME" && "$SKIP_BAR" == false ]]; then
    already_listed=false
    for existing in "${CLONE_BARS[@]}"; do
        if [[ "$existing" == "$JWM_BAR_NAME" ]]; then
            already_listed=true
            break
        fi
    done
    if [[ "$already_listed" == false ]]; then
        CLONE_BARS+=("$JWM_BAR_NAME")
    fi
fi

echo ""
info "========================================="
info " JWM 安装脚本"
info " 构建模式: $BUILD_MODE"
info " JWM Bar (installed only): $JWM_BAR_NAME"
if [[ ${#CLONE_BARS[@]} -gt 0 ]]; then
    info " 拉取仓库: ${CLONE_BARS[*]}"
fi
info " 重新生成配置: $REGEN_CONFIG"
info "========================================="
echo ""

# JWM 只安装到 /usr/local/bin；无论本次是否跳过编译，都先清理旧遗留文件。
remove_jwm_cargo_bins

# 1. 拉取所有 CLONE_BARS 仓库到本地（不编译）
if [[ ${#CLONE_BARS[@]} -gt 0 ]]; then
    for bar in "${CLONE_BARS[@]}"; do
        sync_bar_repo "$bar"
    done
fi

# 2. 仅安装 JWM_BAR_NAME 对应的 bar 到 cargo bin
if [[ "$SKIP_BAR" == false && -n "$JWM_BAR_NAME" ]]; then
    build_bar "$JWM_BAR_NAME"
fi

# 3. 处理 jwm
if [[ "$SKIP_JWM" == false ]]; then
    build_and_install_jwm
    show_jwm_tool_help
fi

# 3. 重新生成配置（可选）
if [[ "$REGEN_CONFIG" == true ]]; then
    regenerate_config
fi

# 4. 同步选中的 bar 到用户配置。放在 --gen-config 之后，避免重新生成配置覆盖选择。
if [[ "$SKIP_BAR" == false && -n "$JWM_BAR_NAME" ]]; then
    sync_selected_bar_config "$JWM_BAR_NAME"
fi

echo ""
ok "全部完成！"
