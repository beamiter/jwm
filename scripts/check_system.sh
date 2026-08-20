#!/bin/bash
# 系统诊断脚本 - 检查运行 JWM 的系统条件

echo "🔍 JWM System Diagnostic"
echo "========================"
echo ""

diagnostic_user=${USER:-$(id -un)}

# 1. 系统信息
echo "📋 System Information:"
uname -a
echo ""

# 2. 用户权限
echo "👤 User Groups:"
groups "$diagnostic_user"
echo ""

# 3. DRM 设备
echo "🖥️  DRM Devices:"
if [ -d /dev/dri ]; then
    ls -lh /dev/dri/
    echo ""

    echo "DRM Status:"
    for card in /sys/class/drm/card*/status; do
        if [ -f "$card" ]; then
            connector=$(basename "$(dirname "$card")")
            status=$(<"$card")
            echo "  $connector: $status"
        fi
    done
else
    echo "  ❌ /dev/dri not found!"
fi
echo ""

# 4. 输入设备
echo "⌨️  Input Devices:"
if command -v libinput &> /dev/null; then
    libinput list-devices 2>/dev/null | head -30
else
    echo "  ⚠️  libinput command not found"
    if [ -d /dev/input ]; then
        find /dev/input -mindepth 1 -maxdepth 1 -printf '  %f\n' | head -10
    else
        echo "  ❌ /dev/input not found"
    fi
fi
echo ""

# 5. Seat 状态
echo "💺 Seat Status:"
if command -v loginctl &> /dev/null; then
    loginctl seat-status seat0 2>/dev/null | head -10 || echo "  No seat0 info available"
else
    echo "  ⚠️  loginctl not available"
fi
echo ""

# 6. 依赖库检查
echo "📦 Required Libraries:"
for lib in libinput libseat libdrm libgbm libEGL libGLESv2; do
    if ldconfig -p | grep -q "$lib"; then
        echo "  ✅ $lib: found"
    else
        echo "  ❌ $lib: NOT FOUND"
    fi
done
echo ""

# 7. 环境变量
echo "🌍 Display/session environment allowlist:"
for variable in DISPLAY WAYLAND_DISPLAY XDG_SESSION_TYPE XDG_CURRENT_DESKTOP XDG_SESSION_DESKTOP DESKTOP_SESSION; do
    if [ -n "${!variable+x}" ]; then
        echo "  $variable=${!variable}"
    else
        echo "  $variable=<unset>"
    fi
done
echo "  JWM_BACKEND=${JWM_BACKEND:-<unset>}"
if [ -n "${JWM_DRM_DEVICE+x}" ]; then
    echo "  JWM_DRM_DEVICE=<set; value omitted>"
else
    echo "  JWM_DRM_DEVICE=<unset>"
fi
echo "  XDG_RUNTIME_DIR=<omitted; doctor checks it without printing it here>"
echo ""

# 8. Rust 工具链
echo "🦀 Rust Toolchain:"
if command -v rustc &> /dev/null; then
    rustc --version
    cargo --version
else
    echo "  ❌ Rust not installed"
fi
echo ""

# 9. JWM 编译状态
echo "🏗️  JWM Build Status:"
cd "$(dirname "$0")/.." || exit 1
if [ -f Cargo.toml ]; then
    echo "  Project: $(grep '^name' Cargo.toml | head -1)"
    echo "  Version: $(grep '^version' Cargo.toml | head -1)"

    if [ -f target/debug/jwm ]; then
        echo "  ✅ Debug build exists"
    else
        echo "  ⚠️  Debug build not found (run: cargo build)"
    fi

    if [ -f target/release/jwm ]; then
        echo "  ✅ Release build exists"
    else
        echo "  ⚠️  Release build not found"
    fi
else
    echo "  ❌ Not in JWM project directory"
fi
echo ""

# 10. JWM 原生启动诊断
echo "🩺 JWM Startup Doctor:"
selected_backend=${JWM_BACKEND:-wayland-udev}
doctor_binary=""
if [ -x target/release/jwm ]; then
    doctor_binary=target/release/jwm
elif [ -x target/debug/jwm ]; then
    doctor_binary=target/debug/jwm
elif command -v jwm &> /dev/null; then
    doctor_binary=$(command -v jwm)
fi

if [ -n "$doctor_binary" ]; then
    echo "  Binary: $doctor_binary"
    echo "  Backend: $selected_backend"
    if "$doctor_binary" --backend "$selected_backend" --doctor; then
        echo "  ✅ Native startup checks completed without blocking errors"
    else
        echo "  ❌ Native startup checks found a blocking error"
    fi
else
    echo "  ⚠️  No jwm binary found; build JWM to run configuration and socket checks"
fi
echo ""

# 11. 权限建议
echo "💡 Recommendations:"
user_groups=$(id -nG "$diagnostic_user")
if [[ " $user_groups " != *" video "* ]]; then
    echo "  ⚠️  User not in 'video' group. Run:"
    echo "     sudo usermod -aG video $diagnostic_user"
    echo "     (then log out and back in)"
fi

if [[ " $user_groups " != *" input "* ]]; then
    echo "  ⚠️  User not in 'input' group. Run:"
    echo "     sudo usermod -aG input $diagnostic_user"
fi

echo ""
echo "✅ Diagnostic complete!"
echo ""
echo "📄 For a privacy-aware report suitable for sharing:"
echo "   jwm-support --backend $selected_backend --offline --output jwm-support.json"
echo "   (This verbose system report is intended for local troubleshooting.)"
