# JWM - Window Manager

用 Rust 编写的窗口管理器，同时支持 X11 和 Wayland 后端。基于 [Smithay](https://github.com/Smithay/smithay) 合成器框架构建。

## 特性

- 多后端支持：X11、Wayland (winit)、Wayland (X11)、Wayland (udev/DRM)
- XWayland 支持
- 可插拔的 Status Bar 系统（15 种实现可选）
- 丰富的窗口管理功能：窗口动画、模糊、阴影、圆角等
- 多显示器支持
- IPC 通信
- `jwm-tool` 管理工具

## 依赖

### 系统库

```bash
# Debian/Ubuntu
sudo apt install libinput-dev libseat-dev libdrm-dev libgbm-dev \
    libegl-dev libgles2-mesa-dev libx11-dev libxcb1-dev \
    libxkbcommon-dev libwayland-dev
```

### 工具链

- Rust (edition 2024)
- cargo
- pkg-config
- clang (bindgen 需要)

## 构建与安装

### 快速安装

```bash
# 默认 release 模式，编译 jwm + xcb_bar
./scripts/install_jwm_scripts.sh

# debug 模式
./scripts/install_jwm_scripts.sh -m debug
```

### 手动构建

```bash
# Release
cargo build --release

# Debug
cargo build

# 启用特定 bar feature
cargo build --release --features xcb_bar
```

### 安装脚本选项

```
用法: install_jwm_scripts.sh [选项]

选项:
  -m, --mode <debug|release>  构建模式（默认: release）
  -b, --bar <bar_name>        选择 status bar（默认: xcb_bar）
  -l, --list-bars             列出所有可用的 bar
  -j, --jobs <N>              并行编译任务数
  --skip-bar                  跳过 bar 编译安装
  --skip-jwm                  跳过 jwm 编译安装（仅编译 bar）
  -h, --help                  显示帮助信息
```

安装脚本会自动将 bar 代码克隆到 `submodules/` 目录，编译后安装到 `/usr/local/bin/`。

### 示例

```bash
# 仅安装 jwm（不装 bar）
./scripts/install_jwm_scripts.sh --skip-bar

# 安装 jwm + egui_bar
./scripts/install_jwm_scripts.sh -b egui_bar

# 仅安装 xcb_bar
./scripts/install_jwm_scripts.sh --skip-jwm

# debug 模式安装全部
./scripts/install_jwm_scripts.sh -m debug -b xcb_bar
```

## Status Bar

JWM 支持多种 status bar 实现，通过 Cargo feature flag 选择：

| Bar | 渲染技术 |
|-----|---------|
| `xcb_bar` | XCB 直接绘制 |
| `x11rb_bar` | x11rb 绑定 |
| `egui_bar` | egui 即时模式 GUI |
| `iced_bar` | Iced 框架 |
| `dioxus_bar` | Dioxus 框架 |
| `gtk_bar` | GTK |
| `relm_bar` | Relm (GTK) |
| `tauri_react_bar` | Tauri + React |
| `tauri_vue_bar` | Tauri + Vue |
| `winit_softbuffer_bar` | winit + softbuffer |
| `winit_pixels_bar` | winit + pixels |
| `winit_wgpu_bar` | winit + wgpu |
| `tao_softbuffer_bar` | tao + softbuffer |
| `tao_pixels_bar` | tao + pixels |
| `tao_wgpu_bar` | tao + wgpu |

所有 bar 的源码托管在 [beamiter](https://github.com/beamiter) 的 GitHub 仓库中，安装脚本会自动拉取到 `submodules/` 目录。

## 后端

| 后端 | 说明 |
|------|------|
| `x11` | 原生 X11 窗口管理 |
| `wayland-winit` | 以 winit 窗口运行的 Wayland 合成器（开发/调试用） |
| `wayland-x11` | 在 X11 窗口中运行的 Wayland 合成器 |
| `wayland-udev` | 直接 DRM/KMS 的 Wayland 合成器（生产环境） |

启动时通过 `jwm-tool daemon --backend <backend>` 或 `JWM_BACKEND` 环境变量选择后端。

## jwm-tool

`jwm-tool` 是 JWM 的管理工具，支持以下子命令：

```
jwm-tool daemon           启动守护进程
jwm-tool daemon --backend wayland-udev  指定后端启动
jwm-tool start            启动 JWM
jwm-tool stop             停止 JWM
jwm-tool restart          重启 JWM
jwm-tool quit             退出
jwm-tool status           查看状态
jwm-tool rebuild          编译并重启 JWM
jwm-tool daemon-check     守护进程健康检查
jwm-tool daemon-restart   重启守护进程
jwm-tool debug            调试信息
```

详见 [tools/README.md](tools/README.md)。

## 项目结构

```
jwm/
├── src/
│   ├── main.rs              # 入口，后端选择
│   ├── lib.rs               # 库导出
│   ├── jwm.rs               # WM 核心逻辑
│   ├── config.rs            # 配置系统
│   ├── ipc.rs               # IPC 通信
│   ├── ipc_server.rs        # IPC 服务端
│   ├── miscellaneous.rs     # 工具函数
│   ├── terminal_prober.rs   # 终端检测
│   ├── core/                # 核心模块（动画、布局等）
│   └── backend/             # 后端实现
│       ├── x11/             # X11 后端
│       ├── wayland_udev/    # Wayland udev 后端
│       ├── wayland_winit/   # Wayland winit 后端
│       └── wayland_x11/     # Wayland X11 后端
├── tools/
│   └── jwm_tool.rs          # jwm-tool 源码
├── scripts/
│   ├── install_jwm_scripts.sh  # 编译安装脚本
│   ├── check_system.sh         # 系统诊断
│   ├── debug_jwm.sh            # 调试启动
│   └── xephyr.sh               # Xephyr 测试
├── submodules/              # bar 源码（自动拉取）
├── benches/                 # 性能基准测试
├── autostart.sh             # 自启动脚本模板
├── jwm-x11.desktop          # X11 session 文件
├── jwm-wayland.desktop      # Wayland session 文件
└── Cargo.toml
```

## 配置

JWM 配置文件位于 `~/.config/jwm/`，支持 TOML 格式，涵盖外观、行为、Status Bar 等配置项。

自启动脚本模板见 [autostart.sh](autostart.sh)，复制到 `~/.config/jwm/autostart.sh` 并按需修改。

## 辅助脚本

- `scripts/check_system.sh` - 检查系统环境是否满足运行要求（DRM、libinput、libseat 等）
- `scripts/debug_jwm.sh` - 以调试模式启动 JWM
- `scripts/xephyr.sh` - 在 Xephyr 中测试运行

## License

MIT
