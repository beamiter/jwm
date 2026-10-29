# 三侧栏实施验收记录

更新于 2026-10-29T09:43:46.584168+00:00。共享宿主、28 个顶栏的双后端功能与外观、固定性能预算和旧配置兼容均已通过。新版真实 ELF 候选包已通过完整生命周期；最终归档使用同组二进制并单独复验。物理 Wayland 另行验收。

本记录使用当前 WM `cbde73f7de2ebe9afc3847fca20040cd8bdc425df0c5fffc1468f013198da079`、共享宿主 `bc2effeb8e564e015e48180fab7dc43566f21779dd830d49b2eb60d61dd06799`。旧版本结果只作历史证据，不替代当前组合。

源码和发布包默认顶栏为 `tauri_leptos_bar`（当前正式二进制 `43bd73da…`），共享宿主仍需显式启用。三侧栏省略模式时均为 `hover`（自动隐藏）；**Super+Shift+B** 在自动隐藏和常驻之间切换并保存，已禁用的边不被开启。源码原生快捷键和旧驻留实例的兼容辅助程序分别有[原生双后端记录](../artifacts/shared-shell/sidebar-mode-native/summary.json)和[兼容双后端记录](../artifacts/shared-shell/sidebar-mode-compat-e2f/summary.json)。本机已[热加载兼容配置](../artifacts/shared-shell/sidebar-autohide/local-shortcut.json)；2026-10-29 15:43 系统重启后的[只读核验](../artifacts/shared-shell/sidebar-autohide/post-reboot-config.json)确认三个 hover、快捷键和辅助程序保留，XCB 配置诊断无错误。这不意味着新程序已对当前桌面执行会话升级。

| 阶段 | 当前状态与证据 |
|---|---|
| P0 契约与基线 | [配置、协议、归属及回滚规则](shared-desktop-shell.md)已建立；[历史原模式基线](../artifacts/shared-shell/baseline/performance.json)与[固定预算](shared-shell-performance-budget.json)保留。 |
| P1 公共策略与绘制 | GUI 独立 core 和独立 Cairo 宿主已实现；[当前旧配置四例](../artifacts/header-alignment/current-release-compat-visual/summary.json)在 XCB/X11RB 分别通过 edge-shell 25 项、four-edge 24 项。 |
| P2 独立宿主 | 独立快照/订阅、有界队列、超时确认、重同步、会话/输出/窗口身份校验及独立应用生命周期有单测与[当前深度验收](../artifacts/header-alignment/current-release-deep-performance/summary.json)。 |
| P3 JWM 监督 | 当前 release 组合双后端各 23 项通过：切顶栏保持 host、顶栏/宿主崩溃恢复、12 秒暂停、真实全屏、任务预览/恢复、逻辑多屏、重启与禁用回滚。 |
| P4 全部前端 | [当前覆盖清单](shared-shell-coverage.json)与[逐项验收](shared-shell-validation.md)记录 28 项实际构建、56 次 XCB/X11RB 通用运行、56 次 sparse tag 交互。每次通用运行另验 TaoGlow 正文材质、外沿及透明圆角；各前端保留自己的渲染框架。 |
| P5 发布与回滚 | [新版真实 ELF 候选包](../artifacts/header-alignment/release-candidate/lifecycle.json)通过新装、升级、回滚、卸载及配置保留；完整 payload、文档、稳定链接和 10 个 ELF 均有校验。[最终归档复验](../artifacts/header-alignment/release-final/lifecycle.json)与[本机安装/激活状态](../artifacts/header-alignment/release-final/local-install.json)独立记录。原驻留会话迁移失败仍保留。 |
| P6 原生 Wayland | [当前 cbde/bc2 与正式 Leptos 43bd 的嵌套原生 10 项](../artifacts/header-alignment/native-wayland-header-final/summary.json)通过，内层 scale=1；[物理矩阵](shared-shell-hardware-acceptance.md)仍未验收。 |

顶栏只显示本 monitor 有 client 或正在选中的 tag，保留原数字；例如 1/3/8 有窗口且空 tag 5 正在选中时显示 1/3/5/8。右键和滚轮仍按原 tag ID 操作，多活跃 tag 分别高亮。

六个 Tauri 顶栏不再强制关闭 WebKit 合成，透明材质按实际 ARGB 表面和合成器能力启用；用户显式选择旧 Cairo 路径或无合成器时使用不透明后备。两种后备都有[实际窗口取样记录](../artifacts/header-alignment/web-material-capability-fallbacks.json)。

当前深度与性能使用 release TaoGlow `bb98aea1…` 和替换用 TaoPixels `1150af6d…`。两例均完成至少三秒内核 `sched:sched_wakeup` 采样，[原预算比较](../artifacts/header-alignment/current-release-deep-performance/budget-comparison.json)全部通过，门限没有事后调整。

| 后端 | CPU（单核%） | RSS 字节 | 唤醒/秒 | 工作区往返 ms | 两次真实任务恢复 ms |
|---|---:|---:|---:|---:|---:|
| XCB | 1.915 | 23,314,432 | 86.949 | 113.375 | 179.568 / 189.760 |
| X11RB | 1.949 | 24,776,704 | 84.791 | 112.903 | 175.728 / 163.718 |

切换顶栏的延迟与各进程内存高水位也在原始 JSON 中保存，没有新增事后门限。[当前外观与兼容记录](../artifacts/header-alignment/current-release-compat-visual/summary.json)含 1600×900 深色和 800×600 浅色合成器截图；这是代表性配置，不声称每个前端已遍历所有尺寸、物理 DPI 和设备组合。当前 X11 UI 验收使用受限 Mesa 软件 GL；不宣称硬件 GPU 已测试。

共享宿主缺音频/背光设备时的不可用状态和私有 D-Bus 媒体动作已验；没有把缺设备测试写成真实音量/亮度硬件调节通过。逻辑 RandR 拓扑、ID 复用和嵌套缩放不等于物理热插拔或原生分数缩放。

原驻留实例迁移的[实际故障记录](../artifacts/shared-shell/release-final/local-install.json)明确显示：IPC restart 返回后旧 JWM/daemon 结束，原 X11 登录会话退出，五个窗口未保留。根因未确认，**不能推断已解决**。[隔离 daemon 重启](../artifacts/shared-shell/daemon-final/env-x11rb-exact/result.json)与[旧文件隔离升级](../artifacts/shared-shell/incident-upgrade-private/summary.json)成功并非同一驻留实例的复验；新 stderr 诊断也不能补回原来丢失的日志。不对当前桌面执行重启或 VT 切换来补证。

[当前只读物理预检](../artifacts/header-alignment/hardware-preflight-current.json)只见内屏连接且物理 seat 被当前 X11 会话占用。专用原生 Wayland 会话、外接显示器、真实分数缩放、物理多屏/热插拔和恢复仍是独立开放项。[完成性审计](shared-shell-completion-audit.md)列出当前逐项证据与历史边界。
