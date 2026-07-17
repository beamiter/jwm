# JWM X11RB / XCB 全功能自动化演示视频方案

> 目标：在真实 X11 桌面会话中，自动完成 JWM x11rb / xcb 后端的功能展示、内置录屏、旁白文本生成、字幕生成与素材整理。  
> 本文档面向后续交给 Codex 落地实现，默认项目仓库为 `beamiter/jwm`。  
> 基准代码版本：仓库 `master` 分支，方案设计时参考提交 `b337fce9eae45218c5ef1219d59b750ab093f9c7` 附近代码。

---

## 1. 项目目标

构建一套可重复执行的 JWM 视频生产系统，使视频录制不依赖人工逐项操作。

最终期望只需要执行：

```bash
python3 video-demo/runner/run_demo.py \
  --backend x11rb \
  --profile full \
  --resolution 1920x1080 \
  --fps 60
```

即可完成：

1. 检查当前真实 X11 会话是否适合录制。
2. 备份当前 JWM 配置和桌面状态。
3. 启动或重启指定后端。
4. 创建稳定、可识别的演示窗口。
5. 按时间线自动展示布局、标签、窗口管理和 compositor 特效。
6. 使用 JWM 自带录屏功能输出独立场景视频。
7. 对每个场景执行 IPC 状态断言和视频完整性检查。
8. 生成旁白文本、字幕时间线、章节信息和剪辑清单。
9. 恢复录制前的配置和桌面状态。
10. 输出成功、失败和需要人工验证的功能矩阵。

完整流程完成后，输出：

```text
video-demo/generated/
├── clips/
│   ├── 001-intro-montage.mp4
│   ├── 010-layout-tile.mp4
│   ├── 011-layout-fibonacci.mp4
│   └── ...
├── narration/
│   ├── 001-intro-montage.txt
│   ├── 010-layout-tile.txt
│   └── ...
├── subtitles/
│   ├── 001-intro-montage.srt
│   └── ...
├── reports/
│   ├── run-report.json
│   ├── feature-matrix.json
│   ├── feature-matrix.md
│   └── environment-report.json
├── editing/
│   ├── concat.txt
│   ├── chapters.ffmeta
│   ├── timeline.csv
│   └── narration.jsonl
├── final-silent.mp4
└── final-with-voice.mp4
```

---

## 2. 核心原则

### 2.1 使用真实 X11 会话

不使用 Xephyr、Xvfb 或其他嵌套 X Server。

原因：

- JWM compositor 在嵌套 X Server 中无法稳定复现真实 GLX、Composite、Present、VSync、VRR 和 GPU 行为。
- 一些视觉特效依赖真实 root window、XComposite redirect、XDamage、GLX framebuffer 和显示器刷新环境。
- 内置录屏需要录制真实 compositor 输出，不能依赖外层窗口的二次合成结果。

所有正式素材必须在真实 X11 登录会话中录制。

### 2.2 一次只录一个场景

不要从头到尾录制一段二十多分钟的视频。

每个功能独立录制成一个 MP4：

```text
一个场景 = 一份配置 + 一组窗口 + 一段操作 + 一段旁白 + 一个视频文件
```

这样可以：

- 单独重录失败功能。
- 调整某一段旁白时无需重录全片。
- x11rb 和 xcb 可以分别重放。
- 新增功能时只增加一个场景。
- 后期剪辑更容易。
- 避免录屏中途失败导致全部素材报废。

### 2.3 优先通过 IPC 驱动

自动化控制分为两类。

第一类使用 `jwm-tool` IPC：

- 布局切换。
- 标签切换。
- 窗口聚焦。
- 窗口排序。
- 浮动、Sticky、PiP、Scratchpad。
- Overview、Magnifier、Peek、Annotation。
- compositor 开关。
- 配置热更新。
- 录屏开始和结束。
- 状态查询。
- 事件订阅。
- 批量配置。
- 批量命令。

第二类使用真实输入模拟：

- 鼠标拖动。
- Snap Preview。
- Wobbly Windows。
- Motion Trail。
- Edge Glow。
- Window Tilt。
- 屏幕标注。
- 必须通过指针轨迹才能表现的交互。

输入模拟首选顺序：

1. XTest。
2. `xdotool`。
3. `ydotool`，仅在 XTest 不足时使用。
4. 不建议依赖桌面环境全局快捷键。

### 2.4 所有运行时修改必须可恢复

真实桌面录制会直接操作当前会话，脚本必须具备事务式恢复能力。

启动时备份：

```text
~/.config/jwm/config_x11.toml
当前后端
当前标签
当前布局
状态栏状态
当前壁纸
现有测试窗口列表
```

退出时无论成功、失败、Ctrl+C 或异常崩溃，都执行：

```text
停止录屏
关闭 demo client
恢复原配置
重新加载配置
恢复原标签和布局
删除临时文件
写入恢复报告
```

Python runner 必须注册：

```python
atexit
SIGINT
SIGTERM
异常捕获
```

还应额外生成恢复命令：

```bash
video-demo/scripts/recover-session.sh
```

出现 runner 被强制杀死时，可以手动执行恢复。

---

## 3. 运行环境要求

### 3.1 必须满足的条件

- 当前登录会话是 X11，而不是 Wayland。
- JWM 正在作为当前窗口管理器运行，或可以安全重启为 JWM。
- compositor 可正常启动。
- `ffmpeg` 和 `ffprobe` 可用。
- `jwm-tool` 可用。
- `xdotool` 或 XTest 驱动可用。
- GPU 驱动支持当前 compositor 路径。
- 屏幕分辨率和缩放在录制期间保持不变。
- 关闭会污染画面的通知、桌面弹窗和自动锁屏。

预检命令建议：

```bash
echo "$XDG_SESSION_TYPE"
echo "$DISPLAY"
xdpyinfo | grep dimensions
glxinfo -B
jwm-tool status
jwm-tool msg get_version
jwm-tool msg get_monitors --raw
jwm-tool msg get_config_status --raw
ffmpeg -encoders | grep -E 'nvenc|vaapi|libx264'
```

### 3.2 推荐录制环境

推荐：

```text
分辨率：1920x1080
刷新率：60 Hz 或固定的高刷新率
缩放：100%
JWM 动画速度：normal
录屏帧率：60 FPS
编码器：NVENC
码率：30M
桌面背景：统一的低干扰壁纸
字体：清晰的等宽字体
状态栏：保留，但需要统一主题
```

如果真实屏幕不是 1920×1080，不强制修改分辨率。runner 应读取实际分辨率，根据比例计算窗口和鼠标轨迹。

所有坐标应使用归一化比例：

```python
x = screen_width * 0.5
y = screen_height * 0.4
```

不要在场景定义中硬编码绝对像素，除非是已知的固定录制机器。

### 3.3 录制前的桌面隔离

因为不使用嵌套 X Server，需要在真实会话中创建“录制专用标签”。

建议：

- 使用最后一个未使用标签作为 `DEMO_TAG`。
- 把所有 demo 窗口放到该标签。
- 开始场景前切换到该标签。
- 临时隐藏或关闭通知中心。
- 开启 Do Not Disturb。
- 禁止自动锁屏和屏保。
- 暂停会弹窗的同步软件。
- 不打开私人终端历史、聊天软件或浏览器页面。

runner 不应主动关闭用户原有窗口，只能隔离 demo 窗口。

建议通过 `WM_CLASS` 识别并管理：

```text
JwmDemo
JwmDemoTerminal
JwmDemoChart
JwmDemoVideo
JwmDemoColor
```

---

## 4. 推荐目录结构

在 JWM 仓库中增加：

```text
video-demo/
├── README.md
├── pyproject.toml
├── manifest/
│   ├── features.toml
│   ├── scenes.toml
│   ├── backends.toml
│   ├── narration.toml
│   └── profiles/
│       ├── smoke.toml
│       ├── layouts.toml
│       ├── compositor.toml
│       └── full.toml
├── runner/
│   ├── __init__.py
│   ├── run_demo.py
│   ├── environment.py
│   ├── session_guard.py
│   ├── jwm_ipc.py
│   ├── jwm_process.py
│   ├── demo_windows.py
│   ├── input_driver.py
│   ├── recorder.py
│   ├── assertions.py
│   ├── timeline.py
│   ├── narration.py
│   └── report.py
├── demo-client/
│   ├── Cargo.toml
│   └── src/
│       ├── main.rs
│       ├── window.rs
│       ├── animation.rs
│       ├── content.rs
│       └── protocol.rs
├── assets/
│   ├── wallpapers/
│   ├── color-tests/
│   ├── icons/
│   └── sample-video/
├── scripts/
│   ├── preflight.sh
│   ├── recover-session.sh
│   ├── disable-interruptions.sh
│   └── restore-interruptions.sh
├── narration/
├── generated/
└── tmp/
```

---

## 5. 需要新增或完善的 JWM 能力

### 5.1 显式录屏 IPC

当前 `toggle_recording` 适合快捷键使用，但不适合自动化视频生产。

建议增加：

```text
start_recording
stop_recording
get_recording_status
```

调用形式：

```bash
jwm-tool msg start_recording \
  --args '{"path":"/home/user/jwm/video-demo/generated/clips/010-layout-tile.mp4"}'

jwm-tool msg stop_recording

jwm-tool msg get_recording_status --raw
```

推荐响应：

```json
{
  "active": true,
  "output_path": "...",
  "segment_path": "...",
  "captured_frames": 180,
  "fps": 60,
  "encoder": "nvenc"
}
```

要求：

- `start_recording` 接受显式文件路径。
- 已经录制时返回错误，不进行隐式切换。
- `stop_recording` 幂等。
- 录制结束后，只有 `ffprobe` 成功才报告完成。
- 保留 `toggle_recording` 兼容现有快捷键。

### 5.2 增加录屏完成事件

建议广播：

```text
recording/started
recording/stopped
recording/finalized
recording/error
```

runner 订阅事件，不依赖固定 sleep。

### 5.3 Tab 功能先完成闭环

当前 IPC 有 `focus_tab`，配置也有 tab bar 相关字段，但窗口管理层的 tab group 查询仍未完成。

正式视频中要演示：

- 创建 Tab Group。
- 将窗口加入 Tab Group。
- 切换 Tab。
- 移出 Tab。
- 关闭当前 Tab。
- Tab Bar 样式。
- 焦点与布局关系。

建议新增 IPC：

```text
tab_group_create
tab_group_add
tab_group_remove
tab_group_focus
tab_group_close_tab
tab_group_dissolve
get_tab_groups
```

在实现和自动化测试完成前，Tab 场景必须标记为：

```text
status = "blocked"
```

不要在正式旁白中声称功能完整。

### 5.4 可选：测试专用 IPC

为了减少依赖 `xdotool`，可以增加仅调试构建启用的 IPC：

```text
demo_set_window_urgent
demo_minimize_window
demo_begin_move
demo_move_window
demo_end_move
demo_trigger_particle_close
```

正式用户功能不需要依赖这些命令，但测试脚本可用它们稳定触发视觉效果。

---

## 6. Demo Client 设计

真实应用启动速度和内容不稳定，必须提供专用演示程序。

示例：

```bash
jwm-demo-client \
  --title "MASTER" \
  --class "JwmDemo" \
  --instance "master" \
  --theme blue \
  --content grid \
  --animate
```

### 6.1 必需能力

- 设置窗口标题。
- 设置 `WM_CLASS` 和 instance。
- 显示大号编号和角色名。
- 可设置背景主题。
- 可设置透明度。
- 可持续播放动画。
- 可显示滚动文本。
- 可显示色卡、渐变、棋盘格。
- 可模拟视频播放。
- 可设置 urgent hint。
- 可创建普通窗口、dialog、popup、透明窗口。
- 可响应 Unix Socket 或 stdin 控制命令。
- 可主动关闭或最小化。
- 可修改标题和颜色。
- 可打印 X11 Window ID。

### 6.2 推荐内容模式

```text
solid       纯色窗口
grid        网格和坐标线
terminal    模拟终端输出
editor      模拟编辑器
chart       动态曲线
video       移动画面
alpha       半透明渐变
color-test  色彩测试卡
text        大号文字标签
```

布局演示统一使用：

```text
MASTER
STACK 1
STACK 2
STACK 3
STACK 4
STACK 5
```

合成器演示使用：

```text
透明窗口
动态视频窗口
高对比色卡
深色背景
白色文本窗口
```

---

## 7. 场景定义格式

推荐使用 TOML 或 YAML。为减少依赖，可优先 TOML。

示例：

```toml
[[scene]]
id = "layout-grid"
order = 120
chapter = "layouts"
title = "Grid 网格布局"
backend = ["x11rb", "xcb"]
duration_hint = 14.0
status = "ready"
narration = """
Grid 布局会根据当前窗口数量，自动计算合适的行数和列数，
让所有窗口尽量均匀地分布在屏幕中。
"""

[scene.environment]
demo_tag = 8
window_count = 6
layout = "tile"

[[scene.setup]]
action = "spawn_demo_windows"
count = 6
content = "grid"

[[scene.setup]]
action = "wait_for_windows"
class = "JwmDemo"
count = 6
timeout = 10.0

[[scene.actions]]
at = 0.0
action = "start_recording"

[[scene.actions]]
at = 1.0
action = "ipc"
command = "setlayout"
args = { layout = "grid" }

[[scene.actions]]
at = 4.0
action = "ipc"
command = "focusstack"
args = { value = 1 }

[[scene.actions]]
at = 7.0
action = "ipc"
command = "movestack"
args = { value = 1 }

[[scene.actions]]
at = 11.0
action = "stop_recording"

[[scene.assertions]]
type = "workspace_layout"
equals = "grid"

[[scene.assertions]]
type = "video_duration"
min = 10.0

[[scene.assertions]]
type = "video_resolution"
match_session = true
```

### 7.1 Action 类型

至少支持：

```text
ipc
set_config
set_config_batch
command_batch
spawn_demo_window
spawn_demo_windows
close_demo_window
wait_for_windows
focus_window
move_pointer
drag_pointer
click
key
type_text
sleep
start_recording
stop_recording
capture_screenshot
show_title_card
show_overlay
assert
```

### 7.2 场景状态

```text
ready
experimental
hardware-dependent
manual-review
blocked
disabled
```

---

## 8. Runner 执行生命周期

### 8.1 全局生命周期

```text
1. 环境预检
2. 创建 session lock
3. 备份配置
4. 记录原桌面状态
5. 启用 DND
6. 关闭屏保和锁屏
7. 选择 demo tag
8. 启动目标后端
9. 等待 IPC 可用
10. 运行场景
11. 输出报告
12. 恢复配置和桌面状态
13. 释放 session lock
```

### 8.2 单场景生命周期

```text
1. 清理上一个场景的 demo 窗口
2. 恢复场景基线配置
3. 切换到 demo tag
4. 创建所需窗口
5. 等待窗口注册
6. 设置初始布局和焦点
7. 应用场景配置
8. 等待 compositor 稳定
9. 开始录屏
10. 按时间轴执行操作
11. 停止录屏
12. 等待 recording/finalized
13. 使用 ffprobe 验证
14. 执行 IPC 状态断言
15. 保存场景报告
16. 关闭 demo 窗口
17. 恢复基线配置
```

### 8.3 不使用固定等待替代状态检查

错误做法：

```python
subprocess.run(...)
time.sleep(2)
```

正确做法：

```text
发送 IPC
等待对应 IPC event
查询 get_windows / get_workspaces
验证状态已经改变
再进入下一步
```

动画可以使用：

```text
配置中的 animation.duration_ms
效果自身 duration
额外安全缓冲 100 到 300 毫秒
```

---

## 9. 功能矩阵

建立 `manifest/features.toml`，作为视频和测试的唯一功能清单。

字段建议：

```toml
[[feature]]
id = "layout-grid"
category = "layout"
title = "Grid"
backend = ["x11rb", "xcb"]
status = "ready"
trigger = "ipc:setlayout"
scene = "layout-grid"
visual = true
state_assertion = "workspace.layout == grid"
notes = ""
```

每个功能记录：

```text
id
中文名
英文名
分类
支持后端
当前状态
触发方式
所需配置
所需窗口
场景 ID
是否需要真实鼠标
是否依赖硬件
是否可自动断言
是否需要人工目视
旁白状态
录制状态
```

最终报告示例：

| 功能 | x11rb | xcb | 自动化 | 视频 | 备注 |
|---|---:|---:|---:|---:|---|
| Tile | 通过 | 通过 | 通过 | 已生成 | |
| Grid | 通过 | 通过 | 通过 | 已生成 | |
| Wobbly | 通过 | 通过 | 通过 | 已生成 | 需真实拖动 |
| Tab | 阻塞 | 阻塞 | 阻塞 | 未生成 | 分组逻辑未闭环 |
| VRR | 待硬件验证 | 待硬件验证 | 部分 | 状态演示 | 不能只靠视频确认 |

---

## 10. 视频章节和场景规划

完整成片建议 24 到 30 分钟。

### 10.1 开场：00:00–00:40

展示快速蒙太奇：

- Cube 工作区切换。
- Wobbly 窗口。
- 背景模糊。
- Overview 3D。
- Particle Close。
- Magnifier。
- Annotation。
- Grid 到 Scrolling 的快速切换。

旁白重点：

- JWM 是一个 Rust 编写的窗口管理器。
- 同时拥有 x11rb 和 xcb 后端。
- 不只做平铺，还自带完整 compositor 和自动化 IPC。

### 10.2 后端与架构：00:40–01:40

展示：

- `JWM_BACKEND=x11rb`
- `JWM_BACKEND=xcb`
- `jwm-tool status`
- `get_version`
- `get_monitors`
- `get_tree`

说明：

- x11rb 为默认 X11 后端。
- xcb 是另一套 X11 transport。
- 窗口管理策略和大部分 compositor 能力共享。
- 视频主要使用 x11rb，结尾使用 xcb 重放验收场景。

### 10.3 窗口管理基础：01:40–04:20

逐项展示：

- 新窗口自动管理。
- 窗口焦点切换。
- 主窗口提升。
- 窗口顺序移动。
- 调整 master factor。
- 调整 client factor。
- 增减 master 数量。
- Tile 与 Floating 切换。
- Sticky。
- PiP。
- Scratchpad。
- Fullscreen。
- Kill Client。
- Toggle Bar。
- Do Not Disturb。
- Session Save / Restore。
- Window Swallowing。

### 10.4 布局系统：04:20–09:40

每种布局 15 到 25 秒：

1. Tile
2. Float
3. Monocle
4. Fibonacci
5. Centered Master
6. Bottom Stack
7. Grid
8. Deck
9. Three Column
10. Tatami
11. Fullscreen Layout
12. Scrolling
13. VStack

每个布局统一：

```text
显示布局名称
切换布局
切换焦点
移动窗口
必要时调整比例
恢复基线
```

重点讲解：

- Tile 的 master / stack。
- Fibonacci 的递归分割。
- Centered Master 的中央主区。
- Deck 的叠放预览。
- Tatami 的组合结构。
- Scrolling 的列式视口。
- VStack 的焦点居中动画。
- Fullscreen Layout 与普通 fullscreen client 的区别。

### 10.5 标签和工作区：09:40–12:00

展示：

- View。
- Loop View。
- Toggle View。
- Tag。
- Toggle Tag。
- Sticky 跨标签。
- 标签专属壁纸。
- 多显示器时 Focus Monitor 和 Tag Monitor。
- 标签切换动画。

工作区 transition：

```text
none
slide
cube
fade
flip
zoom
stack
blinds
```

每种效果使用同一组窗口和同一切换方向，方便对比。

### 10.6 Tab：12:00–13:00

仅在功能完成后启用。

展示：

- 创建 tab group。
- 将三个窗口加入。
- 切换 tab。
- 鼠标点击 tab。
- 关闭 tab。
- 解散 tab group。
- tab bar 主题。

如果功能尚未完成：

- 从正式成片中移除。
- 可在结尾“开发中功能”中展示短预览。
- 不把占位代码描述成已完成功能。

### 10.7 Compositor 基础视觉：13:00–16:20

每个效果都采用 A/B：

```text
效果关闭
停留 1 秒
效果开启
执行窗口操作
停留 2 到 3 秒
```

功能：

- 圆角。
- 阴影。
- Focused / Unfocused 边框。
- Active / Inactive Opacity。
- Inactive Dim。
- 背景 Blur。
- Frosted Glass。
- Fade In / Fade Out。
- Window Scale Animation。
- Focus Highlight。
- Snap Preview。
- Wallpaper Crossfade。
- Per-window opacity rule。
- Per-window corner radius rule。
- Blur exclude。
- Shadow exclude。

### 10.8 高级视觉特效：16:20–21:30

每个效果独立场景：

- Wobbly Windows。
- Motion Trail。
- Genie Minimize。
- Ripple on Open。
- Particle Effects。
- Window Tilt。
- Edge Glow。
- Attention Animation。
- PiP Visual Treatment。
- Overview 3D Prism。
- Expose / Mission Control。
- Peek。
- Zoom to Fit。
- Magnifier。
- WaterLily 全屏流体模拟。
- Annotation。

不要同时打开多个重效果。每次只突出一个。

### 10.9 色彩与无障碍：21:30–23:30

使用专用色卡窗口展示：

- Color Temperature。
- Night Light。
- Saturation。
- Brightness。
- Contrast。
- Invert。
- Grayscale。
- Deuteranopia。
- Protanopia。
- Tritanopia。
- Magnifier。
- Annotation。

### 10.10 工程能力：23:30–25:30

展示：

- Debug HUD。
- Extended Debug HUD。
- Partial Damage。
- FPS 和 frame time。
- Shader Hot Reload。
- Config Hot Reload。
- `set_config`。
- `set_config_batch`。
- `command_batch`。
- IPC 事件订阅。
- 内置录屏。
- Benchmark。
- `get_windows`。
- `get_workspaces`。
- `get_tree`。

### 10.11 硬件相关能力：25:30–26:40

展示状态和检测，不夸大效果：

- Present。
- OML Sync Control。
- VSync。
- Audio Sync。
- Direct Scanout。
- Fullscreen Unredirect。
- VRR。
- HDR。
- Color Management。

规则：

- 能通过状态查询确认的，展示状态。
- 必须真实硬件验证的，标记为 hardware-dependent。
- 不使用肉眼无法证明的画面声称“已经启用并生效”。

### 10.12 xcb 回放验收：26:40–28:00

切换到 xcb 后端，重放精简场景：

- Tile。
- Grid。
- Scrolling。
- 标签切换。
- Blur。
- Wobbly。
- Overview。
- 录屏。
- IPC 查询。

最终显示自动生成的后端功能矩阵。

---

## 11. 鼠标和键盘自动化

### 11.1 输入轨迹必须可复现

拖动路径不要随机。

示例：

```toml
[[scene.actions]]
at = 2.0
action = "drag_pointer"
from = [0.35, 0.28]
path = [
  [0.45, 0.25],
  [0.60, 0.35],
  [0.70, 0.55],
  [0.52, 0.62],
  [0.40, 0.45]
]
duration = 4.0
steps = 120
```

坐标为屏幕比例。

### 11.2 鼠标移动应使用平滑插值

推荐：

```text
线性
ease-in-out
cubic Bézier
```

Wobbly 和 Motion Trail 使用较慢轨迹。

Snap Preview 使用直接拖到屏幕边缘。

Edge Glow 使用移动到边缘并停留。

Tilt 使用在屏幕四角之间移动。

WaterLily 由外部 Julia worker 自主演进，不再需要手势或鼠标轨迹。

### 11.3 录屏中显示按键

建议增加一个简单的按键提示 overlay：

```text
Alt + J
Alt + K
Mod + Space
IPC: setlayout grid
```

overlay 可以由：

- 独立透明 X11 窗口。
- demo client overlay 模式。
- 后期 FFmpeg drawtext。

推荐在后期生成，不要污染 JWM 本身。

---

## 12. 录屏设计

### 12.1 默认录屏参数

```toml
recording_fps = 60
recording_encoder = "nvenc"
recording_bitrate = "30M"
recording_quality = 18
recording_output_dir = "/absolute/path/to/video-demo/generated/clips"
```

### 12.2 录制边界

开始录屏前：

```text
窗口已经创建
布局已经稳定
鼠标移动到不遮挡内容的位置
标题卡准备完成
等待至少一个 compositor frame
```

停止录屏前：

```text
最后一个效果已经结束
额外保留 0.5 到 1 秒静止画面
```

### 12.3 视频完整性检查

每个场景结束后执行：

```bash
ffprobe -v error \
  -show_entries format=duration \
  -show_entries stream=width,height,r_frame_rate,codec_name \
  -of json \
  scene.mp4
```

验证：

- 文件存在。
- 文件大小大于最小阈值。
- 时长合理。
- 分辨率正确。
- 帧率正确。
- 视频流可解码。
- 不存在 0 秒 MP4。
- 不存在未写入 moov atom 的文件。

失败则：

```text
标记失败
保存日志
最多自动重试一次
不继续覆盖旧的成功视频
```

---

## 13. 旁白、字幕和后期

### 13.1 旁白文本

每个场景存一段纯文本：

```text
narration/010-layout-tile.txt
```

同时生成：

```json
{
  "scene": "layout-tile",
  "title": "Tile 主从布局",
  "text": "Tile 是 JWM 最经典的主从布局……",
  "target_duration": 18.0
}
```

### 13.2 本地语音克隆接入

runner 不直接绑定具体语音模型。

定义外部命令模板：

```toml
[voice]
command = [
  "python3",
  "/path/to/tts.py",
  "--text-file", "{text}",
  "--output", "{wav}"
]
```

生成：

```text
generated/voice/010-layout-tile.wav
```

### 13.3 音频时长适配

当旁白比视频长：

1. 优先延长场景尾部静帧。
2. 轻微降低视频速度，范围建议 0.95 到 1.00。
3. 不压缩旁白到不自然的语速。

当旁白比视频短：

1. 在段落前后保留呼吸停顿。
2. 保留环境静音。
3. 不强制填满每一秒。

### 13.4 字幕生成

根据旁白句子自动分段：

```text
每行不超过 18 个汉字
每屏最多两行
标点处优先切分
显示时间不少于 1 秒
```

输出：

```text
SRT
ASS
纯文本脚本
```

### 13.5 合成

最终 FFmpeg 流程：

```text
独立场景 MP4
+ 独立旁白 WAV
+ 字幕
+ 标题卡
= 完整场景片段

所有完整场景片段
= final-with-voice.mp4
```

同时保留：

```text
final-silent.mp4
final-no-subtitles.mp4
final-with-voice.mp4
```

---

## 14. 日志和诊断

每次运行创建：

```text
generated/runs/2026-xx-xx-xxxxxx/
├── runner.log
├── jwm.log
├── ffmpeg.log
├── environment.json
├── config-before.toml
├── config-demo.toml
├── config-after.toml
├── window-tree-before.json
├── window-tree-after.json
└── report.json
```

场景报告：

```json
{
  "scene": "effect-wobbly",
  "backend": "x11rb",
  "success": true,
  "started_at": "...",
  "duration": 15.42,
  "video": "...",
  "assertions": [
    {
      "name": "config enabled",
      "success": true
    },
    {
      "name": "video valid",
      "success": true
    }
  ],
  "manual_review_required": true
}
```

---

## 15. 安全和恢复机制

### 15.1 Session Lock

防止同时运行两个录制任务：

```text
$XDG_RUNTIME_DIR/jwm-video-demo.lock
```

lock 内容：

```json
{
  "pid": 12345,
  "backend": "x11rb",
  "started_at": "...",
  "recovery_script": "..."
}
```

### 15.2 录制时禁止危险命令

runner 默认禁止：

```text
quit
删除用户窗口
修改非 demo 窗口标签
修改系统显示模式
修改真实显示器 HDR metadata
关闭用户应用
```

只有 `--unsafe-hardware-tests` 才允许真实 HDR、VRR、modeset 等实验。

### 15.3 崩溃恢复

runner 必须在每个状态变化后刷新 recovery state：

```json
{
  "recording_active": true,
  "temporary_config_active": true,
  "demo_windows": [123, 456],
  "dnd_changed": true,
  "original_backend": "x11rb"
}
```

`recover-session.sh` 根据该文件恢复。

---

## 16. 实施顺序

### 阶段 1：基础控制

完成：

- Python runner 骨架。
- IPC client。
- 环境预检。
- 配置备份和恢复。
- demo tag 隔离。
- Demo Client 最小版本。
- 显式录屏 IPC。
- 单个 Tile 场景。

验收：

```text
一条命令生成 Tile 布局 MP4
退出后配置完全恢复
```

### 阶段 2：布局和基础窗口管理

完成：

- 13 种布局。
- 聚焦、排序、mfact、cfact、nmaster。
- Floating、Sticky、PiP、Scratchpad。
- 标签和工作区。

验收：

```text
layouts profile 可完整重放
每个场景有状态断言
```

### 阶段 3：Compositor

完成：

- 基础视觉 A/B。
- 鼠标轨迹。
- Wobbly、Motion Trail、Tilt、Edge Glow。
- Overview、Expose、Peek。
- 录屏重试和人工目视标记。

### 阶段 4：旁白和后期

完成：

- narration 文件。
- TTS 外部命令接口。
- SRT。
- FFmpeg 拼接。
- chapter metadata。
- final-silent 和 final-with-voice。

### 阶段 5：xcb 验收和 Tab

完成：

- xcb 精简回放。
- 后端功能矩阵。
- Tab Group 完整实现。
- Tab 场景。
- 最终 full profile。

---

## 17. Codex 首批任务拆分

建议按以下顺序交给 Codex。

### Task 1：新增显式录屏 IPC

要求：

- `start_recording(path)`
- `stop_recording`
- `get_recording_status`
- 保留现有 `toggle_recording`
- 增加单元测试
- 增加 `jwm-tool` help

### Task 2：创建 Demo Client

要求：

- Rust X11 客户端。
- 可设置 title/class/instance/theme/content。
- 输出窗口 ID。
- 支持持续动画。
- 支持 Unix Socket 控制。
- 支持 urgent、close、minimize、title change。

### Task 3：实现 Runner 基础设施

要求：

- Python 3.11+。
- 不依赖嵌套 X Server。
- 检查真实 X11。
- session guard。
- 配置备份恢复。
- IPC client。
- 场景 TOML 解析。
- ffprobe 验证。
- 日志和报告。

### Task 4：实现第一个端到端场景

场景：

```text
Tile 布局
6 个 demo window
切换焦点
调整 mfact
移动窗口
JWM 内置录屏
状态断言
输出 MP4
```

### Task 5：扩展全部布局

实现所有布局场景，并生成布局章节剪辑清单。

### Task 6：实现鼠标输入驱动

要求：

- 相对坐标。
- 平滑移动。
- Drag。
- Click。
- 重试。
- 屏幕边界保护。

### Task 7：实现 compositor 场景

先完成：

```text
blur
rounded corners
shadow
fade
wobbly
motion trail
overview
magnifier
annotation
```

### Task 8：完成 Tab Group

先分析现有 tab compositor 数据结构，再补全窗口管理状态和 IPC。

---

## 18. MVP 验收标准

第一版不要求立即完成所有功能。

MVP 必须满足：

- 真实 X11 会话运行。
- 不破坏原桌面配置。
- 可自动创建并清理 demo 窗口。
- 可通过 IPC 控制 JWM。
- 可显式开始和停止 JWM 内置录屏。
- 可生成 Tile、Grid、Scrolling、Wobbly、Overview 五个场景。
- 每个场景都有独立 MP4。
- 视频经过 ffprobe 验证。
- 失败场景可单独重跑。
- 运行结束后桌面恢复。
- 生成 feature matrix 和 run report。

MVP 命令：

```bash
python3 video-demo/runner/run_demo.py \
  --backend x11rb \
  --profile smoke
```

---

## 19. 最终命令设计

### 预检

```bash
python3 video-demo/runner/run_demo.py --preflight
```

### 录制单场景

```bash
python3 video-demo/runner/run_demo.py \
  --backend x11rb \
  --scene effect-wobbly
```

### 录制布局章节

```bash
python3 video-demo/runner/run_demo.py \
  --backend x11rb \
  --profile layouts
```

### 完整录制

```bash
python3 video-demo/runner/run_demo.py \
  --backend x11rb \
  --profile full
```

### xcb 验收

```bash
python3 video-demo/runner/run_demo.py \
  --backend xcb \
  --profile backend-smoke
```

### 只生成旁白和字幕

```bash
python3 video-demo/runner/run_demo.py \
  --generate-narration \
  --generate-subtitles
```

### 合成最终视频

```bash
python3 video-demo/runner/run_demo.py \
  --assemble \
  --voice
```

### 恢复桌面

```bash
bash video-demo/scripts/recover-session.sh
```

---

## 20. 最终交付标准

项目完成后应包含：

- 可重复执行的真实 X11 自动录制工具。
- 完整 feature matrix。
- x11rb 完整视频素材。
- xcb 后端验收素材。
- 每个场景独立旁白。
- 每个场景独立字幕。
- 最终剪辑时间线。
- 无配音完整视频。
- 有配音完整视频。
- 自动恢复脚本。
- 一份失败和人工验证报告。

最重要的约束：

```text
不要为了“自动化”牺牲真实 compositor 行为。
不要把未实现功能写成已完成功能。
不要通过固定 sleep 假装状态已经完成。
不要直接操作或关闭用户原有窗口。
不要让一次场景失败破坏整次录制。
```

本方案的第一优先级是：

```text
显式录屏 IPC
→ Demo Client
→ 真实 X11 Session Guard
→ Tile 端到端 MVP
→ 全布局
→ Compositor 特效
→ 旁白与最终合成
```
