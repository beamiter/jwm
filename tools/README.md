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
