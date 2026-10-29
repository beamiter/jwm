# 共用三侧栏完成性审计

更新于 2026-10-29T09:43:46.584168+00:00。**28项顶栏、当前深度行为、固定性能预算、旧配置兼容和新版真实 ELF 候选包生命周期已通过。** 最终归档独立复验；原驻留会话迁移失败和物理Wayland未验收不因其他成功记录而消失。

当前证据使用 WM `cbde73f7…`、host `bc2effeb…`。本页主表反映追加验收后的有效状态；[早期44项审计JSON](../artifacts/header-alignment/completion-audit.json)保存初始时间戳、历史二进制和追加记录，不能把其中早期前端数量快照或旧的整表统计当成当前完成声明。完整交付上下文见[实施验收记录](shared-shell-delivery.md)。

| 阶段 | 当前有效结论 |
|---|---|
| P0 | 配置、归属、协议、清单和基线已建立，性能门限在基线时固定。 |
| P1 | 公共策略/绘制已实现；当前旧配置四例均通过。 |
| P2 | 独立host、独立IPC、有界资源与身份校验有实现、单测及当前深测。 |
| P3 | 当前 release XCB/X11RB 各23项深测通过，包含切换、崩溃、暂停、重启、任务、全屏、多屏逻辑和回滚。 |
| P4 | [当前覆盖清单](shared-shell-coverage.json)包含28项构建、56次通用运行、56次sparse交互；56次通用运行另验正文材质、外沿与透明圆角。 |
| P5 | [当前真实候选包生命周期](../artifacts/header-alignment/release-candidate/lifecycle.json)通过；[最终归档复验](../artifacts/header-alignment/release-final/lifecycle.json)单独保存。原驻留迁移仍失败且原因未定。 |
| P6 | 当前cbde/bc2嵌套原生10项通过；实体显示、外屏、原生分数缩放和物理热插拔仍开放。 |

当前性能比较已通过[原P0预算](../artifacts/header-alignment/current-release-deep-performance/budget-comparison.json)：XCB/X11RB CPU为1.915%/1.949%，RSS为23,314,432/24,776,704字节，真实内核唤醒86.949/84.791次每秒，工作区往返113.375/112.903ms，四次真实任务恢复163.718–189.760ms。没有事后调整门限。

三侧栏省略模式默认hover；Super+Shift+B持久切换常驻/隐藏，disabled边保持禁用。[源码原生模式](../artifacts/shared-shell/sidebar-mode-native/summary.json)、[旧实例兼容模式](../artifacts/shared-shell/sidebar-mode-compat-e2f/summary.json)、[本机热加载](../artifacts/shared-shell/sidebar-autohide/local-shortcut.json)及[系统重启后只读核验](../artifacts/shared-shell/sidebar-autohide/post-reboot-config.json)分别提供证据。

## 当前逐项证据

`proven`表示实现及针对性契约测试；`current`表示所述范围内的当前组合实测；`historical`只支持历史版本；`pending`仍缺最终证据；`contradicted`保存已观察到的失败。单项通过不表示整个阶段或全部硬件已通过。

### P0

| 项目 | 有效分类 | 当前证据与边界 |
|---|---|---|
| P0.1 固定 28 前端清单、声明后端、实际命令/二进制/截图/失败原因 | proven | 28 项清单、命令/二进制/依赖/失败字段已建立；当前合并状态单独见 P4。 |
| P0.2 固定配置优先级、新旧互斥、独立回滚、会话宿主所有权和协议 | proven | 配置与协议实现、目标接口变化及对应单测已核对；见[协议说明](shared-desktop-shell.md)和[机器可读审计](../artifacts/header-alignment/completion-audit.json)。 |
| P0.3 保存原固定四栏与悬停截图和真实交互基线 | historical | [原固定/悬停基线](../artifacts/shared-shell/baseline/)保留，属于计划基线。 |
| P0.4 P0 预先固定 CPU/RSS/内核唤醒/延迟预算 | proven | [P0 固定预算](shared-shell-performance-budget.json)保留；当前比较见 P4.9。 |
| P0.5 执行前核对计划基线与当前接口差异 | proven | 配置与协议实现、目标接口变化及对应单测已核对；见[协议说明](shared-desktop-shell.md)和[机器可读审计](../artifacts/header-alignment/completion-audit.json)。 |

### P1

| 项目 | 有效分类 | 当前证据与边界 |
|---|---|---|
| P1.1 公共几何、显隐状态机、场景、命中、动作与向内预览锚点；core 无 GUI 依赖 | proven | [当前 core 测试](../artifacts/shared-shell/sidebar-mode-native/core-tests.log)、[host 测试](../artifacts/header-alignment/host-all-tests.log)和实现契约；core 无 GUI 窗口依赖，队列、超时与资源有界。 |
| P1.2 旧固定四栏和悬停入口继续确定性单测与真实兼容回归 | current | [当前旧配置四例](../artifacts/header-alignment/current-release-compat-visual/summary.json)：edge-shell/four-edge 双后端通过，覆盖新旧互斥、模式迁移和失败配置保持。 |
| P1.3 相同三侧栏绘制/字体/玻璃/图标；保留各顶栏框架配置 | current | 共享侧栏实现与[native21](../artifacts/header-alignment/native21-current-final.json)、[Dioxus](../artifacts/header-alignment/dioxus-material-final/summary.json)一致性已验；Tauri6 最终材质合并仍见 P4。 |

### P2

| 项目 | 有效分类 | 当前证据与边界 |
|---|---|---|
| P2.1 宿主独立进程，可独立运行/退出，不依赖顶栏事件循环 | current | [当前 release 双后端各23项](../artifacts/header-alignment/current-release-deep-performance/summary.json)覆盖宿主独立/唯一、重连身份、顶栏替换、崩溃/暂停/重启、任务锚点、占位/strut、全屏及退出回滚。 |
| P2.2 独立 IPC 全量快照后有界订阅，不竞争顶栏共享队列 | proven | [当前 core 测试](../artifacts/shared-shell/sidebar-mode-native/core-tests.log)、[host 测试](../artifacts/header-alignment/host-all-tests.log)和实现契约；core 无 GUI 窗口依赖，队列、超时与资源有界。 |
| P2.3 版本、会话、输出身份、修订号；乱序/重复/断线/初始竞态可恢复 | current | [当前 release 双后端各23项](../artifacts/header-alignment/current-release-deep-performance/summary.json)覆盖宿主独立/唯一、重连身份、顶栏替换、崩溃/暂停/重启、任务锚点、占位/strut、全屏及退出回滚。 |
| P2.4 可替换状态合并；点击不被高频滑动挤掉；动作 ACK/超时/重试资源有界 | proven | [当前 core 测试](../artifacts/shared-shell/sidebar-mode-native/core-tests.log)、[host 测试](../artifacts/header-alignment/host-all-tests.log)和实现契约；core 无 GUI 窗口依赖，队列、超时与资源有界。 |
| P2.5 Dock 会话/投影/窗口代次校验；过期窗口拒绝；预览锚定实际面板向内展开 | current | [当前 release 双后端各23项](../artifacts/header-alignment/current-release-deep-performance/summary.json)覆盖宿主独立/唯一、重连身份、顶栏替换、崩溃/暂停/重启、任务锚点、占位/strut、全屏及退出回滚。 |
| P2.6 音量亮度媒体统一采样分发；缺设备禁用；应用启动不随 host 回收 | current | 当前深测验证统一 provider、设备缺失不可用、私有 D-Bus 媒体和宿主退出后应用存活；真实音量/亮度硬件调节没有条件，未宣称通过。 |

### P3

| 项目 | 有效分类 | 当前证据与边界 |
|---|---|---|
| P3.1 每会话仅 1 受监督 host；每输出 3 面板与必要触发窗；拒绝重复 | current | [当前 release 双后端各23项](../artifacts/header-alignment/current-release-deep-performance/summary.json)覆盖宿主独立/唯一、重连身份、顶栏替换、崩溃/暂停/重启、任务锚点、占位/strut、全屏及退出回滚。 |
| P3.2 切换/崩溃/重启顶栏不重建共享 host；TaoGlow/Pixels 互换 | current | [当前 release 双后端各23项](../artifacts/header-alignment/current-release-deep-performance/summary.json)覆盖宿主独立/唯一、重连身份、顶栏替换、崩溃/暂停/重启、任务锚点、占位/strut、全屏及退出回滚。 |
| P3.3 开启共享模式禁用 Tao 私有 companion；保留皮肤/布局；退出模式回滚 | current | [当前旧配置四例](../artifacts/header-alignment/current-release-compat-visual/summary.json)：edge-shell/four-edge 双后端通过，覆盖新旧互斥、模式迁移和失败配置保持。 |
| P3.4 新表优先；旧 four-edge 映射三边 always；旧 edge_shell 逐边映射；报告来源 | current | [当前旧配置四例](../artifacts/header-alignment/current-release-compat-visual/summary.json)：edge-shell/four-edge 双后端通过，覆盖新旧互斥、模式迁移和失败配置保持。 |
| P3.5 共享模式 enabled 默认 false；主栏固定 top，关闭后恢复旧单边位置 | proven | 共享宿主仍显式启用；主栏在共享模式固定顶部；[当前兼容四例](../artifacts/header-alignment/current-release-compat-visual/summary.json)保留回滚与旧配置入口。 |
| P3.6 常驻占位、悬停覆盖、平铺/最大化避让、外部 strut 去重 | current | [当前 release 双后端各23项](../artifacts/header-alignment/current-release-deep-performance/summary.json)覆盖宿主独立/唯一、重连身份、顶栏替换、崩溃/暂停/重启、任务锚点、占位/strut、全屏及退出回滚。 |
| P3.7 启动配置输出→握手登记→映射几何；准备占位、失败撤销、退出清理 | current | [当前 release 双后端各23项](../artifacts/header-alignment/current-release-deep-performance/summary.json)覆盖宿主独立/唯一、重连身份、顶栏替换、崩溃/暂停/重启、任务锚点、占位/strut、全屏及退出回滚。 |
| P3.8 崩溃退避恢复、健康检查/reap；12 秒 WM 暂停不误杀 | current | [当前 release 双后端各23项](../artifacts/header-alignment/current-release-deep-performance/summary.json)覆盖宿主独立/唯一、重连身份、顶栏替换、崩溃/暂停/重启、任务锚点、占位/strut、全屏及退出回滚。 |
| P3.9 真实客户端全屏、逐 tag 显隐、锁定、整体禁用与 workspace 联动 | current | [当前 release 双后端各23项](../artifacts/header-alignment/current-release-deep-performance/summary.json)覆盖宿主独立/唯一、重连身份、顶栏替换、崩溃/暂停/重启、任务锚点、占位/strut、全屏及退出回滚。 |
| P3.10 输出/会话 ID，稳定显示器偏好；拔插清理预览；ID 复用拒绝陈旧动作 | current | 当前深测已验逻辑输出、负坐标/身份复用及锁定；物理多屏/插拔仍属于 P6 开放项。 |
| P3.11 诊断给出 PID/backend/version/panels/modes/reservations/sync/error | current | [当前 release 双后端各23项](../artifacts/header-alignment/current-release-deep-performance/summary.json)覆盖宿主独立/唯一、重连身份、顶栏替换、崩溃/暂停/重启、任务锚点、占位/strut、全屏及退出回滚。 |

### P4

| 项目 | 有效分类 | 当前证据与边界 |
|---|---|---|
| P4.1 28 项实际编译与 XCB/X11RB 接入，含命令、二进制、依赖、截图和错误原因 | current | [当前覆盖清单](shared-shell-coverage.json)：28项构建及56次通用运行，逐项核对当前二进制哈希、构建日志、非空JSON及可解码截图。 |
| P4.2 TaoGlow 对齐顶栏，原 tag 数字显示 occupied∪selected，保留空活跃 tag | current | 全28项双后端sparse验证occupied∪selected、空活跃tag、原数字身份，且正文材质、外沿与圆角均有合成器截图检查。 |
| P4.3 点击/右键/滚轮按真实 tag ID，当前 monitor 多活跃状态，最小化也算 occupied | current | 全28项共56次sparse交互验证原ID点击、多选右键、滚轮、保留空tag5及有窗口tag1/3/8；最小化窗口仍计入occupied。 |
| P4.4 固定壁纸浅/深、1280x800/1600x900、小屏/DPI/缺设备的合成器截图 | current | [当前代表性截图](../artifacts/header-alignment/current-release-compat-visual/summary.json)：1280×800浅色、1600×900深色、800×600小屏；不声称每前端遍历物理DPI/硬件。 |
| P4.5 不同图形后端保持正确透明度与圆角，按能力降级 | current | 全28项双后端通用截图均检查TaoGlow正文材质、外沿和透明圆角；原生ARGB与无合成器/显式Cairo降级另有对应记录。当前使用Mesa软件图形，未宣称实机GPU通过。 |
| P4.6 通用夹具之外单独深测应用/工作区/媒体/任务恢复/overflow/hover preview | current | [当前 release 双后端各23项](../artifacts/header-alignment/current-release-deep-performance/summary.json)覆盖宿主独立/唯一、重连身份、顶栏替换、崩溃/暂停/重启、任务锚点、占位/strut、全屏及退出回滚。 |
| P4.7 慢速进边、透明间隙、离开取消、外部抓取、拖动到外部释放 | historical | [原hover行为基线](../artifacts/shared-shell/baseline/hover-passing.log)保留完整慢移/抓取/间隙回归；当前深测另验逐边hover与延迟显隐，未冒称重新遍历旧基线全部物理指针组合。 |
| P4.8 负坐标、逻辑热插拔、身份复用、输出位置/缩放，每输出唯一角色 | current | 当前深测已验逻辑输出、负坐标/身份复用及锁定；物理多屏/插拔仍属于 P6 开放项。 |
| P4.9 固定预算下记录 CPU/RSS/内核 wakeups/交互延迟/切换内存峰值 | current | [当前固定预算比较](../artifacts/header-alignment/current-release-deep-performance/budget-comparison.json)双后端全部通过；内核唤醒、CPU/RSS、交互延迟及切换峰值均有原始数据。 |

### P5

| 项目 | 有效分类 | 当前证据与边界 |
|---|---|---|
| P5.1 源码安装、manifest/version/协议检查、宿主缺失诊断、CI归档、文档 | proven | 源码安装/格式兼容/必需程序检查有测试与构建日志；stub安装测试不替代新版真实包验收。 |
| P5.2 真实 ELF 新装/升级/回滚/卸载与用户配置保留 | current | [新版真实候选包](../artifacts/header-alignment/release-candidate/lifecycle.json)完成10个ELF及116个payload文件、文档树、稳定链接与回滚复验；[最终归档](../artifacts/header-alignment/release-final/lifecycle.json)重复同一完整流程。 |
| P5.3 一键关闭共享、清除面板/触发区/工作区预留，回到原顶栏 | current | [当前 release 双后端各23项](../artifacts/header-alignment/current-release-deep-performance/summary.json)覆盖宿主独立/唯一、重连身份、顶栏替换、崩溃/暂停/重启、任务锚点、占位/strut、全屏及退出回滚。 |
| P5.4 省略三边模式默认 auto-hide；快捷键切换常驻/隐藏并持久化，disabled不被开启 | proven | 三边默认hover、Super+Shift+B持久切换、保留disabled边；[原生](../artifacts/shared-shell/sidebar-mode-native/summary.json)/[旧实例兼容](../artifacts/shared-shell/sidebar-mode-compat-e2f/summary.json)和[重启后只读配置](../artifacts/shared-shell/sidebar-autohide/post-reboot-config.json)分别保存。 |
| P5.5 真实原驻留会话升级保留五个应用/登录会话 | contradicted | [原驻留会话迁移失败](../artifacts/shared-shell/release-final/local-install.json)：登录会话退出、五窗口未保留；隔离成功不是同一实例复验，根因未确认。 |
| P5.6 daemon stderr/退出码等故障诊断可追溯 | proven | [新daemon诊断](../artifacts/shared-shell/daemon-diagnostics-private/result.json)保存stderr/退出状态；不能补回原事件丢失的stderr或据此推断故障解决。 |
| P5.7 核心和宿主测试/严格 lint，失败记录不删除 | proven | 新功能定向core/host/策略测试与构建检查有效；7198/5061/417大套件属历史版本，未拼接成当前全量通过。 |

### P6

| 项目 | 有效分类 | 当前证据与边界 |
|---|---|---|
| P6.1 独立原生 panel surfaces、anchors/input/exclusive zone、native/XWayland混合客户端 | current | [cbde/bc2与正式Leptos43bd嵌套原生10项](../artifacts/header-alignment/native-wayland-header-final/summary.json)验证layer-shell、独占区、混合客户端及生命周期；内层scale1，非物理支持声明。 |
| P6.2 Wayland stalled bootstrap 可及时取消，不无限阻塞/restart | proven | [Wayland bootstrap取消/期限测试](../artifacts/header-alignment/wayland-bootstrap-tests.log)通过；不替代实际物理输出验收。 |
| P6.3 物理 DRM/KMS 启动、真实分数缩放、外屏混合、多屏负坐标、热插拔、恢复 | pending | [当前只读物理预检](../artifacts/header-alignment/hardware-preflight-current.json)及[硬件矩阵](shared-shell-hardware-acceptance.md)仍开放；需要专用会话与外屏，不触碰当前桌面。 |

## 历史证据与未完成边界

初始审计在2026-10-29T08:42Z逐项读取了基线、旧性能、旧deep、旧配置、core/host测试、daemon、真实旧包等非空JSON/日志和图片，并核对旧归档SHA与ELF。随后补入当前deep/性能、legacy四例、dark/small和各材料修复；原始分类及版本区别保存在[审计JSON](../artifacts/header-alignment/completion-audit.json)。历史7198/5061/417测试、e2f/907包和早期覆盖统计未改写成当前组合结果。

原驻留会话升级导致登录会话退出、五个窗口未保留，见[原始故障](../artifacts/shared-shell/release-final/local-install.json)。[隔离daemon成功](../artifacts/shared-shell/daemon-final/env-x11rb-exact/result.json)、[旧文件私有升级成功](../artifacts/shared-shell/incident-upgrade-private/summary.json)及新日志诊断都不足以证明同一故障已解决。

当前原生Wayland结果属于nested winit、内层scale1，使用当前正式Leptos、host和WM，不证明其他顶栏的原生Wayland或实体GPU。音量/背光无设备路径不等于真实硬件调节。物理矩阵仍开放；原驻留会话升级故障未被隔离成功替代。
