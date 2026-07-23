# jwm-tool

JWM 管理工具（单二进制多子命令）。

## 构建与安装

```bash
# 通过安装脚本（推荐）
./scripts/install_jwm_scripts.sh

# 手动构建
cargo build --release
sudo install target/release/jwm-tool /usr/local/bin/
```

## 使用方法

### 启动守护进程

```bash
jwm-tool daemon

# 指定 JWM 可执行文件路径
jwm-tool daemon --jwm-binary /path/to/jwm

# 通过环境变量指定
JWM_BINARY=/path/to/jwm jwm-tool daemon

# 指定运行后端
jwm-tool daemon --backend wayland-udev

# 通过环境变量指定后端
JWM_BACKEND=wayland-udev jwm-tool daemon
```

### 控制命令

```bash
jwm-tool start      # 启动 JWM
jwm-tool stop       # 停止 JWM
jwm-tool restart    # 重启 JWM
jwm-tool status     # 查看状态
jwm-tool quit       # 退出
```

### 守护进程管理

```bash
jwm-tool daemon-check     # 守护进程健康检查/自动重启
jwm-tool daemon-restart   # 重启守护进程
```

### 构建并重启 JWM

```bash
jwm-tool rebuild --jwm-dir /path/to/jwm

# 或通过环境变量
JWM_DIR=/path/to/jwm jwm-tool rebuild
```

### 调试信息

```bash
jwm-tool debug
```

### 嵌套后端冒烟矩阵

在现有桌面里以私有 `XDG_RUNTIME_DIR` 启动嵌套开发后端
（`wayland-winit` / `wayland-x11`,以及 Xephyr 内的 `x11rb` /
`xcb`），按固定矩阵依次验证:启动、IPC 健康、配置重载、窗口生命
周期、截图能力、策略场景、干净退出。每一步都有显式超时;失败时保
留唯一一份日志目录并在报告中给出可执行的下一步。

```bash
jwm-tool nested-smoke                 # 按宿主会话自动选择全部后端
jwm-tool nested-smoke --backend winit # 只测 wayland-winit
jwm-tool nested-smoke --backend x11-transports # 只测 x11rb+xcb 及差分
jwm-tool nested-smoke --json          # 版本化 JSON 报告 (schema_version 1)
jwm-tool nested-smoke --save          # 保存到 $XDG_RUNTIME_DIR/jwm-smoke
jwm-tool nested-smoke --client foot   # 指定窗口生命周期客户端
jwm-tool nested-smoke --keep          # 通过时也保留运行目录与日志
```

退出码:`0` 全部通过(跳过不算失败)、`1` 有步骤失败或传输差分不
一致、`2` 宿主会话没有可测的嵌套后端。窗口生命周期步骤按会话协议
自动探测客户端(Wayland:foot、weston-terminal、alacritty、kitty、
adwaita-1-demo、gtk4-demo;X11:xterm、xclock、xeyes);截图步骤在
Wayland 行按 `get_capture_status` 的真实能力决定执行或跳过(嵌套
后端不服务帧捕获,如实记 skip),在 X11 行用 `xwd` 抓取 Xephyr
根窗口验证像素可读。

**x11rb vs xcb 差分**:两个 X11 传输在各自的 Xephyr 里执行同一段
固定 IPC 场景(映射客户端 → 切 tag → 切回 → 切浮动),每一步截取
一份归一化可观测状态快照(窗口 class/tags/浮动/几何 + 工作区布局/
占用;传输相关 id 与异步标题按定义排除)。任一快照不一致即整个矩
阵失败,并指明第几个快照、哪个部分发生分歧。

### 守护进程日志

守护进程日志写入 `~/.local/share/jwm/jwm_daemon.log`，并做有界轮转：
单代上限 1 MiB，超限后当前文件重命名为 `jwm_daemon.log.1`（替换旧的
上一代）。因此磁盘占用始终限制在约两代以内，长时间运行不会无限增长。

长期部署建议改用 journald 管理日志——由 systemd 用户服务或
`systemd-run` 启动守护进程即可，标准输出会进入 journal，并由
journald 统一负责持久化、限额与清理：

```bash
systemd-run --user --unit=jwm-daemon jwm-tool daemon
journalctl --user -u jwm-daemon -f
```

## WaterLily 着色器检查

安装 `glslangValidator` 后，可独立编译检查 X11 WaterLily 后处理着色器：

```bash
python3 tools/validate_waterlily_shaders.py
```
