#!/bin/bash
# 嵌套(nested)运行 jwm 的 Wayland 后端,方便在现有桌面里测试。
#
# 用法:
#   scripts/run_nested.sh [winit|x11] [debug|release]
#
# 示例:
#   scripts/run_nested.sh            # = winit + release(默认)
#   scripts/run_nested.sh winit      # winit 后端(X11 或 Wayland 桌面均可)
#   scripts/run_nested.sh x11        # wayland-on-x11 后端(宿主必须是 X11)
#   scripts/run_nested.sh winit debug
#
# 环境变量(可覆盖):
#   RUST_LOG       默认 info        日志级别
#   WAYLAND_DEBUG  默认 0           设 1 打印 Wayland 协议流量

set -euo pipefail

cd "$(dirname "$0")/.."

# ---- 解析参数 ----------------------------------------------------------------
backend_arg="${1:-winit}"
profile="${2:-release}"

case "$backend_arg" in
    winit|wayland-winit) JWM_BACKEND="wayland-winit" ;;
    x11|wayland-x11)     JWM_BACKEND="wayland-x11" ;;
    *)
        echo "❌ 未知后端 '$backend_arg';可选 winit | x11" >&2
        exit 1
        ;;
esac

case "$profile" in
    release) build_flag="--release"; bin="target/release/jwm" ;;
    debug)   build_flag="";          bin="target/debug/jwm" ;;
    *)
        echo "❌ 未知 profile '$profile';可选 debug | release" >&2
        exit 1
        ;;
esac

# ---- 前置检查 ----------------------------------------------------------------
# wayland-x11 后端依赖 smithay 的 X11 backend,宿主必须是 X11 会话。
if [ "$JWM_BACKEND" = "wayland-x11" ] && [ -z "${DISPLAY:-}" ]; then
    echo "❌ wayland-x11 后端需要 X11 宿主(未检测到 \$DISPLAY)。" >&2
    echo "   在 Wayland 桌面里请改用:scripts/run_nested.sh winit" >&2
    exit 1
fi

if [ -z "${DISPLAY:-}" ] && [ -z "${WAYLAND_DISPLAY:-}" ]; then
    echo "❌ 既无 \$DISPLAY 也无 \$WAYLAND_DISPLAY,似乎不在图形会话里。" >&2
    exit 1
fi

# ---- 构建 --------------------------------------------------------------------
echo "🔧 构建 jwm ($profile) ..."
cargo build $build_flag --bin jwm

# ---- 运行 --------------------------------------------------------------------
export JWM_BACKEND
export RUST_LOG="${RUST_LOG:-info}"
export RUST_BACKTRACE="${RUST_BACKTRACE:-1}"
export WAYLAND_DEBUG="${WAYLAND_DEBUG:-0}"

log_file="/tmp/jwm_${backend_arg}_$(date +%s).log"

echo ""
echo "🚀 启动:JWM_BACKEND=$JWM_BACKEND  profile=$profile"
echo "   宿主:DISPLAY=${DISPLAY:-<none>}  WAYLAND_DISPLAY=${WAYLAND_DISPLAY:-<none>}"
echo "   日志:$log_file"
echo "   它会开一个窗口;窗口里可启动 Wayland 客户端测试,例如:"
echo "     WAYLAND_DISPLAY=wayland-1 foot      # socket 名见启动日志"
echo ""

exec "$bin" 2>&1 | tee "$log_file"
