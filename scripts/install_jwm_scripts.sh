#!/bin/bash
# install_jwm_scripts.sh - 编译并安装 JWM 及 status bar
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
    tauri_react_bar
    tauri_vue_bar
    winit_pixels_bar
    winit_softbuffer_bar
    winit_wgpu_bar
    x11rb_bar
    xcb_bar
)

# ============================================================
# 默认值
# ============================================================
BUILD_MODE="release"
BAR_NAME="xcb_bar"
SKIP_BAR=false
SKIP_JWM=false
JOBS=""

# ============================================================
# 帮助信息
# ============================================================
usage() {
    cat <<EOF
用法: $(basename "$0") [选项]

选项:
  -m, --mode <debug|release>  构建模式（默认: release）
  -b, --bar <bar_name>        选择要编译安装的 status bar
  -l, --list-bars             列出所有可用的 bar
  -j, --jobs <N>              并行编译任务数（传给 cargo）
  --skip-bar                  跳过 bar 编译安装
  --skip-jwm                  跳过 jwm 编译安装（仅编译 bar）
  -h, --help                  显示此帮助信息

示例:
  $(basename "$0")                          # release 模式编译安装 jwm（不编译 bar）
  $(basename "$0") -m debug                 # debug 模式编译安装 jwm
  $(basename "$0") -b xcb_bar              # release 模式编译安装 jwm + xcb_bar
  $(basename "$0") -b xcb_bar --skip-jwm   # 仅编译安装 xcb_bar
  $(basename "$0") -m debug -b egui_bar    # debug 模式编译安装 jwm + egui_bar
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
            BAR_NAME="$2"
            # 验证 bar 名称
            valid=false
            for bar in "${ALL_BARS[@]}"; do
                if [[ "$bar" == "$BAR_NAME" ]]; then
                    valid=true
                    break
                fi
            done
            if [[ "$valid" == false ]]; then
                err "未知的 bar: $BAR_NAME"
                info "使用 --list-bars 查看所有可用的 bar"
                exit 1
            fi
            shift 2
            ;;
        -l|--list-bars)
            list_bars
            ;;
        -j|--jobs)
            JOBS="$2"
            shift 2
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
    CARGO_BUILD_FLAG="--release"
    TARGET_DIR="target/release"
else
    CARGO_BUILD_FLAG=""
    TARGET_DIR="target/debug"
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
# 编译并安装 bar
# ============================================================
build_and_install_bar() {
    local bar="$1"
    local bar_dir="$SUBMODULES_DIR/$bar"

    if [[ ! -f "$bar_dir/Cargo.toml" ]]; then
        err "$bar_dir/Cargo.toml 不存在，无法编译"
        exit 1
    fi

    info "编译 $bar（$BUILD_MODE 模式）..."
    # shellcheck disable=SC2086
    cargo build $CARGO_BUILD_FLAG $CARGO_JOBS --manifest-path "$bar_dir/Cargo.toml"

    local bin_path="$bar_dir/$TARGET_DIR/$bar"
    if [[ ! -f "$bin_path" ]]; then
        err "编译产物未找到: $bin_path"
        exit 1
    fi

    info "安装 $bar -> /usr/local/bin/$bar"
    sudo install "$bin_path" /usr/local/bin/
    ok "$bar 安装完成"
}

# ============================================================
# 编译并安装 JWM
# ============================================================
build_and_install_jwm() {
    info "编译 jwm（$BUILD_MODE 模式）..."

    local feature_flag=""
    if [[ -n "$BAR_NAME" ]]; then
        feature_flag="--features $BAR_NAME"
        info "启用 feature: $BAR_NAME"
    fi

    cd "$PROJECT_ROOT"
    # shellcheck disable=SC2086
    cargo build $CARGO_BUILD_FLAG $CARGO_JOBS $feature_flag

    info "安装 jwm, jwm-tool -> /usr/local/bin/"
    sudo rm -f /usr/local/bin/jwm /usr/local/bin/jwm-tool
    sudo install "$TARGET_DIR/jwm" /usr/local/bin/
    sudo install "$TARGET_DIR/jwm-tool" /usr/local/bin/
    ok "jwm, jwm-tool 安装完成"

    info "安装 desktop 文件 ..."
    [[ -f jwm-x11.desktop ]] && sudo install jwm-x11.desktop /usr/share/xsessions/
    [[ -f jwm-wayland.desktop ]] && sudo install jwm-wayland.desktop /usr/share/wayland-sessions/
    ok "desktop 文件安装完成"
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
echo ""
info "========================================="
info " JWM 安装脚本"
info " 构建模式: $BUILD_MODE"
[[ -n "$BAR_NAME" ]] && info " Status Bar: $BAR_NAME"
info "========================================="
echo ""

# 1. 处理 bar
if [[ -n "$BAR_NAME" && "$SKIP_BAR" == false ]]; then
    sync_bar_repo "$BAR_NAME"
    build_and_install_bar "$BAR_NAME"
fi

# 2. 处理 jwm
if [[ "$SKIP_JWM" == false ]]; then
    build_and_install_jwm
    show_jwm_tool_help
fi

echo ""
ok "全部完成！"
