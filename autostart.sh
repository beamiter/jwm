#!/bin/sh

# =========================================================
# JWM Autostart Script
# 路径: ~/.config/jwm/autostart.sh
# =========================================================

# --- 函数：如果程序未运行则启动 ---
run_if_not_running() {
    if ! pgrep -f "$1" > /dev/null; then
        "$@" &
    fi
}

# 如果你是虚拟机或多显示器，在这里取消注释并配置
# xrandr --output Virtual-1 --mode 1920x1080
# xrandr --output HDMI-1 --right-of eDP-1 --auto
xrandr --output HDMI-1 --rotate normal --left-of eDP-1 --auto &

# 将主音量和耳机音量设置为 80% 并取消静音
if command -v amixer > /dev/null; then
    amixer sset Master 70 unmute > /dev/null 2>&1
    amixer sset Headphone 70 unmute > /dev/null 2>&1
fi

# 对于中文用户非常重要
export GTK_IM_MODULE=fcitx
export QT_IM_MODULE=fcitx
export XMODIFIERS=@im=fcitx
# run_if_not_running fcitx -d
run_if_not_running fcitx5 -d

echo "[autostart.sh] Initialization finished."
