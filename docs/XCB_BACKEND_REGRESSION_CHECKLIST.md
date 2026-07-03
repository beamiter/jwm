# XCB Backend Regression Checklist

本清单用于验证 `xcb` 后端在功能和行为上是否与 `x11rb` 后端保持对齐。

建议执行方式：

1. 分别在 `JWM_BACKEND=x11rb` 和 `JWM_BACKEND=xcb` 下完整跑一遍。
2. 每个用例都记录两边结果，不要只记录 `xcb`。
3. 差异按三类标记：`功能缺失`、`行为不一致`、`性能/稳定性回退`。
4. 如用例依赖 compositor，请分别测试 `JWM_COMPOSITOR=0` 和 `JWM_COMPOSITOR=1`。

建议记录格式：

| 用例 | x11rb | xcb | 结果 | 备注 |
|------|-------|-----|------|------|
| 示例：Alt-Tab 切换焦点 | PASS | PASS | 一致 | 无 |

结果标记建议：

- `PASS`：符合预期
- `FAIL`：功能错误或缺失
- `DIFF`：两后端都能工作，但行为有差异
- `N/A`：当前环境不适用

## P0 必测

### 启动与 WM 接管

- [ ] `JWM_BACKEND=x11rb` 能正常启动并接管 root window。
- [ ] `JWM_BACKEND=xcb` 能正常启动并接管 root window。
- [ ] 在已有 WM 运行时，两后端都能正确拒绝接管，而不是挂死或异常退出。
- [ ] 两后端启动日志中无明显初始化错误、扩展探测错误或事件循环异常。

### 基本窗口生命周期

- [ ] 新开普通窗口时，两后端都能正确管理该窗口。
- [ ] 关闭窗口时，两后端都能正确移除窗口，无残留边框或幽灵窗口。
- [ ] 最小化和恢复窗口行为一致。
- [ ] 应用主动退出时，窗口销毁路径一致，无崩溃或状态残留。
- [ ] 已存在窗口场景下重启 JWM，两后端都能正确重新接管已有客户端。

### 焦点与输入

- [ ] 点击窗口可正确切换焦点。
- [ ] `Alt-Tab`/键盘切换焦点行为一致。
- [ ] 键盘输入能正确送达聚焦窗口。
- [ ] 鼠标点击、拖动、滚轮在常见应用中行为一致。
- [ ] `WM_TAKE_FOCUS` 客户端能被正确激活。
- [ ] `_NET_ACTIVE_WINDOW` 被正确维护，任务栏或外部工具读取结果一致。

### 移动与缩放

- [ ] 鼠标拖动移动窗口时，窗口位置更新正确。
- [ ] 从上、下、左、右边缘 resize 行为正确。
- [ ] 从四个角 resize 行为正确。
- [ ] 释放鼠标后，最终几何与拖动过程显示一致。
- [ ] 拖动和 resize 过程中不会卡住、跳变、丢失 grab 或留下错误光标。

### 堆叠与层级

- [ ] 焦点切换时窗口提升顺序正确。
- [ ] dialog、popup、menu、tooltip 的覆盖层级正确。
- [ ] 置顶和置底状态在两后端表现一致。
- [ ] fullscreen 窗口与普通窗口的堆叠关系一致。

### 工作区与多显示器

- [ ] 切换 tag/workspace 后，可见窗口集合正确。
- [ ] 窗口在 workspace 间移动后状态正确。
- [ ] `_NET_CURRENT_DESKTOP` 和桌面数量、桌面名称维护一致。
- [ ] 多显示器下窗口能在正确输出上显示。
- [ ] 窗口跨显示器移动时位置、焦点、激活状态一致。
- [ ] 不同分辨率和刷新率输出能被正确枚举。

## P1 EWMH / ICCCM / 兼容性

### EWMH 属性

- [ ] `_NET_CLIENT_LIST` 在两后端都正确维护。
- [ ] `_NET_CLIENT_LIST_STACKING` 在两后端都正确维护。
- [ ] `_NET_WM_STATE_FULLSCREEN` 行为一致。
- [ ] `_NET_WM_STATE_MAXIMIZED_VERT/HORZ` 行为一致。
- [ ] `_NET_WM_STATE_ABOVE` / `_NET_WM_STATE_BELOW` 行为一致。
- [ ] `_NET_WM_STATE_STICKY` / `SKIP_TASKBAR` / `DEMANDS_ATTENTION` 行为一致。
- [ ] `_NET_CLOSE_WINDOW` 外部触发时行为一致。
- [ ] `_NET_RESTACK_WINDOW` 外部触发时行为一致。

### ICCCM / 常见属性

- [ ] `WM_HINTS` 读取和处理一致。
- [ ] `WM_NORMAL_HINTS` 对初始尺寸、最小尺寸、增量尺寸的处理一致。
- [ ] `WM_CLASS` 规则匹配行为一致。
- [ ] `WM_TRANSIENT_FOR` 对话框跟随与聚焦逻辑一致。
- [ ] modal 对话框的焦点与层级行为一致。

### 特殊窗口类型

- [ ] `override_redirect` 菜单窗口不会被错误纳入普通管理。
- [ ] tooltip、dropdown、popup menu 的显示和销毁逻辑一致。
- [ ] dock / panel / utility / splash / notification 窗口类型处理一致。
- [ ] 输入法候选窗、截图选择框等临时窗口不会触发错误管理。

## P1 Systray / XEmbed

- [ ] 两后端都能正确尝试获取 systray selection owner。
- [ ] 若已有其他 tray owner，两后端都能优雅放弃而不是异常。
- [ ] 常见托盘图标可以成功嵌入。
- [ ] 托盘图标 map/unmap/destroy 行为正确。
- [ ] 托盘图标刷新、闪烁、动态更新时显示正常。
- [ ] `XEMBED_INFO` 变化能被正确处理。
- [ ] 托盘菜单弹出、关闭、重新聚焦行为正常。

## P1 Compositor 基线

### 开关与初始化

- [ ] `JWM_COMPOSITOR=0` 时，两后端都能正常运行在非合成模式。
- [ ] `JWM_COMPOSITOR=1` 时，两后端都能正常初始化 compositor。
- [ ] compositor 初始化失败时，两后端都能优雅降级，不影响基本 WM 功能。
- [ ] overlay window 不会被误当成普通客户端。

### 基本视觉效果

- [ ] 阴影效果一致。
- [ ] 透明窗口显示正确。
- [ ] 圆角效果一致。
- [ ] 模糊效果在支持场景下正常。
- [ ] 焦点切换后的激活/非激活视觉状态一致。
- [ ] 新开、关闭、切换窗口的基础动画无明显差异。

### 运行时行为

- [ ] 在已有窗口场景下运行时启用 compositor，旧窗口都能被正确纳入合成。
- [ ] 运行时关闭 compositor 后显示恢复正常，无残留重定向问题。
- [ ] compositor overlay/self-capture 不会形成反馈循环。

## P2 Present / 渲染时序

- [ ] Present 扩展可用时，两后端都能正确注册事件。
- [ ] 高频刷新窗口下无明显卡顿、黑帧或挂起。
- [ ] `CompleteNotify` 路径正常工作。
- [ ] `IdleNotify` 路径正常工作。
- [ ] 视频播放窗口在两后端下表现一致。
- [ ] benchmark `start/stop/report/auto-exit` 都能正常使用。
- [ ] 启用 partial damage 后，无明显脏区残影、拖影或漏刷。

## P2 高级 Compositor 功能

### Overview / Expose / Thumbnail

- [ ] Overview 模式可正常进入和退出。
- [ ] Overview 中窗口缩略图位置正确。
- [ ] Overview 选中高亮与激活逻辑正确。
- [ ] Expose 模式可正常进入和退出。
- [ ] Expose 点击能正确返回目标窗口。
- [ ] live thumbnail / capture thumbnail 输出内容正确。

### 交互与视觉增强

- [ ] magnifier 开关正常，鼠标跟随正常。
- [ ] peek 模式表现一致。
- [ ] snap preview 位置和尺寸正确。
- [ ] tag 切换动画在两后端行为一致。
- [ ] edge glow、urgent、PiP 等视觉状态切换正常。
- [ ] dock position、overview monitor、multi-monitor overview 行为一致。

### 注释、录制与辅助功能

- [ ] annotation mode 可正常开启和关闭。
- [ ] annotation 新建 stroke、追加点、连续绘制行为正常。
- [ ] recording 开始和停止行为正常。
- [ ] color temperature、brightness、contrast、saturation 调节生效。
- [ ] grayscale、invert colors、colorblind mode 行为正常。

## P2 输出能力

- [ ] 输出枚举结果在两后端一致，包括名称、坐标、尺寸、缩放、刷新率。
- [ ] 主显示器刷新率探测结果一致。
- [ ] 多显示器布局变化后，输出信息能正确更新。
- [ ] VRR capability 查询结果一致。
- [ ] VRR 开关调用失败模式或成功模式一致。
- [ ] HDR metadata 探测与设置路径行为一致。

## P3 稳定性与边角场景

- [ ] Flameshot 一类 overlay/screenshot 工具不会被错误管理。
- [ ] 大量窗口场景下，overview、Alt-Tab、workspace 切换保持稳定。
- [ ] 高频开关窗口时无明显泄漏、崩溃或事件风暴。
- [ ] 托盘图标频繁刷新时 CPU 占用无异常升高。
- [ ] compositor 开关反复切换后无状态污染。
- [ ] Xephyr 或嵌套 X 环境下，失败场景的降级行为一致。

## 差异记录

记录建议：

- 差异类型：`功能缺失` / `行为不一致` / `性能回退` / `稳定性问题`
- 复现条件：应用、分辨率、显示器数量、是否启用 compositor
- 影响范围：仅 `xcb`，还是两后端都存在
- 日志位置：相关启动日志、事件日志、崩溃信息

| 编号 | 用例 | 差异类型 | 现象 | 复现条件 | 结论 |
|------|------|----------|------|----------|------|
| 1 |  |  |  |  |  |

## 回归结论

- [ ] `xcb` 后端可作为 `x11rb` 后端的功能等价替代。
- [ ] 所有 P0 用例通过。
- [ ] 所有 P1 用例通过，或已有可接受差异说明。
- [ ] 所有已知差异已记录并可稳定复现。
- [ ] 若仍有 blocker，禁止将 `xcb` 视为完全对齐。
