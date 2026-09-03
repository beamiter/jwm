# Handoff

滚动记录待办：每条写清楚「现状 / 缺口 / 落点」，让下一次接手的人不必重新考古。

---

## 2026-09-03：UI/UX 九轮（网格 v2 拖拽换 tag + live 缩略 / HDR P0-2）

1. **网格总览拖窗口换 tag**：按住 cell 里的窗口线框拖到另一 cell 松开 =
   把窗口移到目标 tag（替换语义，与 dwm `tag()` 一致，经 `move_client_to_tag`
   走 EWMH/arrange 全链路），面板保持打开。提交时机从 press 改为 release：
   纯状态机 `plan_press`/`plan_release`（tags_overview.rs:172/206）——同 cell
   松开 = view 跳转（原点击语义），跨 cell 且越界激活 = 移动窗口，scrim 松开
   仅解除手势不取消面板。WM 侧快照保留窗口身份（`window_ids` 与线框平行，
   不过 API 边界）；线框命中 `tags_grid::frame_window_at`（topmost，含最小线宽）。
   已知限制：grab 的 release 不带键号，按下 button-1 后任意键 release 都会
   结算手势（注释写明）。嵌套实测：拖到 tag3 → IPC 确认 tags 位变化且面板
   仍在；无位移点击仍跳转。
2. **当前 tag cell live 缩略**：`TagsGrid.live: Option<LiveTagsCell>`（cell 索引 +
   `(WindowId, 归一化rect)`，rect 与线框同一映射零漂移；id 越界先例 = expose）。
   两端 `render_tags_grid_live_cell` 复用 expose 缩略图的 shader 路径把窗口纹理
   逐帧画进 cell（缺纹理回退线框），非当前 tag 保持线框（停放窗口纹理是旧画面，
   画出来是误导）。X11 端 id 在委托宏翻译（未知句柄丢弃，arrange 重建自愈）。
   实测 live 证据：xclock 秒针在 cell 内逐秒更新（裁片 AE diff 212/2.5s，
   线框 cell diff 0）。damage 未动：system UI 打开本就连帧。
3. **HDR P0 第 2 项（逐类外部元素颜色计划）**：见下方 TODO 节的
   「进展（2026-09-03）」小节——`external_elements_color_pipeline_safe` 总 bool
   拆成五类（cursor/DnD/lock/layer-top/layer-overlay）可诊断计划，组装/route/
   诊断三方同源；修掉「未 commit 的 DnD/lock/layer 树也触发 fallback」的
   false-positive；IPC 新增 `external_elements` 逐类状态。HDR enable 仍
   fail-closed。

验证：fmt / clippy -D warnings / 两组 cargo check 全绿；`cargo test --locked`
lib 2561 passed / 0 failed；v2a/v2b 均有嵌套实测截图（/tmp/jwm-uivalidate/shots/
v2a_*.png、v2b_*.png）。

**网格总览剩余**：跨显示器同框；urgent 标记（v1.1 设计里留的 v2 项）。

---

## 2026-09-03：UI/UX 八轮（嵌套实测验证 + 三个实测 bug 修复）

前七轮全部经嵌套会话（Xephyr :80 + x11rb + GLX + xdotool 注入）实测，
截图与 IPC 证据在 /tmp/jwm-uivalidate/shots/。expose 键盘导航、Alt+Tab
松手提交/Escape 取消/最小化恢复、tags 网格键盘+鼠标、toast 悬停暂停/点击
关闭/动作按钮、开窗动画中间帧全部 PASS。实测抓出并已修三个 bug：

1. **X11 tab 条窗口增删后不重绘（根因在 WM 侧投递门禁，不在合成器）**。
   `render_pending_frame` 先过 `compositor_needs_render()` 门再投递 groups，
   但开窗/关窗帧走 `tick_animations`（渲染但从不投递 groups 且消耗
   needs_render）——groups 全会话只在第一次 hover 时碰巧送进去一次。修复：
   groups 投递改为变更门控的 `sync_window_groups()`（`jwm/rendering.rs`），
   提到门禁之前、并接入 tick_animations 两个渲染点；`pushed_window_groups`
   缓存保证静止桌面零多余推送（有单测断言）。wayland 无恙的原因是其
   `compositor_needs_render` 还或了 smithay 的 `needs_redraw`。
2. **root 事件掩码缺 BUTTON_MOTION 与 BUTTON_RELEASE，tab 拖拽换序在 X11
   永不生效**。tab 拖拽刻意不走 pointer grab，X 协议下按住按钮的
   MotionNotify 与 ButtonRelease 都要显式选择。`EventMaskBits` 补
   `BUTTON_MOTION = 1<<11`（common_define.rs:290），root mask
   （navigation.rs:1090）补 BUTTON_RELEASE + BUTTON_MOTION，xcb/x11rb 掩码
   映射各补一位（各有映射单测）。实测拖拽最左格到最右格 → 平铺序正确重排，
   `commit_window_tab_reorder` 日志命中。普通窗口拖拽走主动 grab（掩码自带）
   不受影响；wayland motion 恒投递天然无恙。
3. **toast 重叠假象与几何收口**。报告的「两条 toast 完全重叠」在当前代码
   不可复现（叠放累加逻辑自 d4d7c0c 起正确），最可能是并行 WIP 构建的瞬态
   损坏。防御性收口：两端内联的叠放算术抽成 `toast::stack_start`/
   `stack_next`/`STACK_GAP` 共享纯函数（2/3/4 张卡 × 有无 OSD 的单调不重叠
   单测）。`docs/notifications.md` 位置表述纠正为「bar 下方居中 dock，
   dynamic island 式」，清掉三处 stale「top-right」注释。

非阻塞观察（未修）：嵌套环境无 nerd font 时面板图标/箭头显示 `?`（字体
回退链问题，非代码 bug）；`jwm-tool msg --help` 清单漏列 `notify`；
`get_workspaces` 的 `tag_index` 是 0 基。

验证：fmt / clippy -D warnings / 两组 cargo check 全绿；`cargo test --locked`
lib 2536 passed / 0 failed；嵌套冒烟矩阵（x11rb/xcb 全步骤）+ 场景实测均过。

---

## 2026-09-03：UI/UX 七轮（网格总览鼠标支持 v1.1）

1. **tags 网格总览支持鼠标**：hover 移动选中（cell 间死区不动键盘选中）、
   左键点 cell 提交（与回车同路径）、点 scrim 取消、panel 死区吞掉不穿透。
   决策是纯函数 `tags_overview::plan_click`（Commit/Cancel/Keep）；命中走 WM 侧
   `tags_grid::grid_geometry` + `cell_at`（viewport/cols 与渲染同源：同一
   `system_ui_viewport()` 推送，有一致性测试钉住），两端渲染器仍不注册命中几何。
2. **指针抓取参数化**：`prepare_system_ui` 的 bool 升级为
   `SystemUiPointerGrab::{None, Buttons, ButtonsAndMotion}`（toggles.rs:46-77），
   12 个既有面板逐一核对为 `Buttons`，tags overview 用 `ButtonsAndMotion`
   （hover 需要 POINTER_MOTION，expose 同款 mask），keybinding viewer 与
   window switcher 为 `None`。释放统一在 `close_system_ui`，confirm/cancel/
   toggle-off/hand-over backstop 全汇一处；编排测试断言抓取 mask 与 ungrab 计数。

验证：fmt / clippy -D warnings / 两组 cargo check 全绿；`cargo test --locked`
lib 2530 passed / 0 failed。

---

## 2026-09-03：UI/UX 六轮（tags 网格总览）

1. **工作区网格总览 v1**（Mod1+O / `toggle_tags_overview`，IPC 同名，开关
   `behavior.tags_overview_enabled` 默认 true）。GNOME 式「一次看到所有 tag」：
   中央网格每格一个 tag 的窗口布局线框（真实几何归一化到工作区、clamp 0..=1），
   occupied/空、active（accent 内框）、选中（chip 填充+`SELECTED_SCALE` 放大）
   三层视觉正交。方向键 clamp 移动（复用 expose 的 `move_expose_selection`，
   与 expose 同手感）、Return 提交（走 `view()` 全链路）、数字键 1-9 直跳、
   Escape 取消、再按 Mod1+O 关闭。旧配置无 Mod1+O 绑定时回填（占用则 warn
   不抢键）。
2. **承载面是 SystemUiOverlay 新 `tags_grid` 字段**（filmstrip 先例）：几何/命中/
   状态全在共享模块（新建 `compositor_common/tags_grid.rs`，geometry 含
   cell 不出 panel/互不重叠/负 origin/cell_at 往返回归；WM 侧新建
   `jwm/features/tags_overview.rs`），两端渲染器只剩 dumb fill/stroke。两端
   均不注册命中几何（v1 键盘-only）。
3. **快照坑（后来者勿踩）**：非当前 tag 的窗口被停放到屏幕外，线框必须取
   `client.geometry.hidden_restore_rect`；sticky 的 tags 会被 `update_sticky_tags`
   改写，快照按 flag 特判「在所有 tag 上」；minimized/swallowed 不进线框但计
   `occupied`（对齐 bar 的 `calculate_tag_masks`）。arrange 末尾
   `refresh_tags_overview` 重建快照，选中按 tag index 天然稳定，commit 时
   `commit_mask` 对 tags_length 重校验（配置 reload 收缩退化为取消）。
4. **设计勘误**：`expose_grid_cols` 对 n=1 在 16:9 屏给 2 列，`grid_cols`
   包装时 clamp 到 `[1, count]`，否则单 tag 时 WM 行走与绘制形状不一致
   （有一致性测试钉住）。

**v1.1/v2 遗留**：~~鼠标 hover/click~~（v1.1 已完成，见七轮；命中走 WM 侧共享
几何，两端仍不注册命中几何）、跨显示器同框、~~当前 tag cell 升级 live 纹理~~
（v2b 已完成）、拖窗口跨 cell 改 tag。文档
`docs/tags-overview.md`。

验证：fmt / clippy -D warnings / 两组 cargo check 全绿；`cargo test --locked`
lib 2525 passed / 0 failed。真机未实测。

---

## 2026-09-03：UI/UX 五轮（切换器恢复最小化窗口 / VRR 文档纠偏）

1. **Alt+Tab 切换器纳入最小化窗口**。`switcher_eligible` 不再排除 hidden
   （最小化客户端保留 tags 且留在 `monitor_stack`，MRU 自然交错，不沉底），
   行尾带 "[minimised]" 标记（复用 `launcher::window_row` 的既有标记）。
   提交走三态纯函数 `commit_disposition` → `Focus` / `RestoreAndFocus` /
   `Cancel`；恢复复用 `set_client_minimized(false)`（dock/launcher 同一条
   含 reverse Genie 的路径），恢复后照旧 focus+restack。手势中途窗口死掉或
   tag 失效降级为取消。docs/window-switcher.md 已同步。
2. **VRR 调查结论与纠偏**（纯文档/注释，无行为变更）。X11 下 per-output
   VRR 开关**结构性不可行**：X server 持 DRM master，RandR 只暴露只读的
   connector `vrr_capable`，不存在 X 协议级控制点；两个 X11 后端显式返回
   `Unsupported` 已是诚实最优。X11 的真实 VRR 故事是 `fullscreen_unredirect`
   + 驱动侧配置（amdgpu `VariableRefresh`）。修正三处失实表述：
   `config.rs:567` 注释（X11 侧只驱动 HUD/metrics）、`tearing_control.rs`
   头注（tearing hint 从未被 page-flip 消费，仅遥测/IPC 可见）、
   `docs/compatibility.md` 新增 VRR 小节（配置键 + 两后端差异）。
   **潜在后续项**（未做）：wayland_udev 的 tearing hint map 已在手，若要让
   游戏真正 async flip，需要在 page-flip 路径消费 hint——这是行为变更，
   需要真机 VRR 显示器验证，不纳入纯代码轮次。

验证：fmt / clippy -D warnings / 两组 cargo check 全绿；`cargo test --locked`
lib 2502 passed / 0 failed。

---

## 2026-09-03：UI/UX 四轮（会话 v3 平铺顺序 / 文档补齐 / zoom_to_fit 修正）

1. **会话格式 v3：精确平铺顺序跨重启保留**。`SessionSnapshot` 新增
   `monitor_orders`（每显示器按序的 `{class, instance}` 身份列表，沿用现有匹配键，
   不持久化 WindowId/title）；保存时从 `monitor_clients` 导出，恢复时在统一 arrange
   前经纯函数 `plan_order_restore` 重排（忽略大小写、出现次数感知、缺失身份跳过、
   新窗口保持相对序追加、浮动分组不变量优先于保存序）。v1→v2→v3 迁移链，
   `tests/fixtures/session_v3.json` 冻结。限制：只覆盖 save/restore 显式工作流
   （裸 seamless exec 仍靠 V1 属性，不含平铺顺序）；class+instance 全同的窗口按
   出现次序配对。dock 顺序不动（已由 `_JWM_MINIMIZED_RESTORE_V1` 属性持久化）。
2. **文档补齐**：新建 `docs/window-tabs.md`、`docs/expose.md`、
   `docs/window-switcher.md`（含 Alt+Tab 默认键位变更的醒目标注与恢复旧行为的
   配置示例）；更新 notifications.md（toast 交互+动作按钮）、ui-theme.md
   （字体栈与自绘 surface 清单）、launcher.md（交叉引用）、minimized-dock.md
   （顺序限制已解）、architecture.md 与 README.md（迁移链与新页面登记）。
   所有配置键名/默认值均对照 src/config.rs 核实。
3. **`compositor_zoom_to_fit` 修正 id 翻译**（window_tabs 剩余项 #4 关闭）：
   API 签名从裸 `Option<u32>` 改为 `Option<WindowId>`（api.rs:2511），X11 委托处
   经 `self.ids.x11()` 翻译（未知句柄 = 不缩放而非错配裸值），wayland 侧字段
   从 `Option<u32>` 更正为本机 `Option<u64>`。仍无调用者，但下一次接线不会再
   踩到 id 错配。注意：wayland 渲染端只存不使用（render.rs 无引用），实际缩放
   只有 X11 实现——接线时若需要 wayland 缩放要补 render 路径。

验证：fmt / clippy -D warnings / 两组 cargo check 全绿；`cargo test --locked`
lib 2501 passed / 0 failed。

---

## 2026-09-03：UI/UX 三轮（标签字体统一 / toast 动作按钮 / 动画默认开）

1. **Expose/overview 标签迁移到统一字体栈，位图字体退役**。两端
   `overview.rs` 的 `render_title_to_pixels`（6x10 ASCII 字模，中文显示 `?`/空格）
   已删除，`x11/compositor/font.rs` 整文件移除；标签改走
   `compositor_font::render_ui_text_to_rgba`（ab_glyph + fc-match + CJK/emoji 回退），
   样式与 tab 条同源：`ui.card` 药丸 chip 底 + `ui.title_ink`，字号约 17.6px。
   纹理缓存粒度不变（进入 overview 时一批创建，静止零上传）。
   **expose 网格本来就不画窗口标题**，无迁移面。至此用户可见文字全部走真字体栈，
   window_tabs 剩余项 #2（非 ASCII 标题）关闭——仅剩 compositor_font 里给
   无 fontconfig 环境留的兜底位图表。
2. **Toast 动作按钮**。`ToastNotification` 增加 `actions`（`NotificationAction`
   挪入 api.rs，历史文件格式不变）与 `notification_id`；有动作的卡片底部画
   至多 3 个 chip 按钮（accent 描边、hover 换 accent wash、加高 34px），
   按钮矩形逐帧进 `toast_rects`（升级为 `ToastRects { id, card, buttons }`，
   纯函数 `hit_test`/`action_row_layout` 有单测）。`compositor_click_toast`
   返回 `ToastClick::{Miss, Dismissed, Action { notification_id, action_key }}`；
   WM 命中 Action 走 `invoke_notification_action` 既有链路（IPC 广播 +
   Dismissed 关闭 record，与通知中心一致）。淡出中的卡再点按钮只吞不重复
   触发（防双击）。toast sanitize 对 actions 有上限/清洗。
3. **`window_animation` 默认开启**（`config.rs:795` serde default_true +
   Default impl 同步），样式默认 `scale`，配合上一轮的三样式；修掉
   `transition_mode` 文档注释里过时的「none 默认」（实际默认 coverflow）。
   用户配置里的显式 false 不受影响。

验证：fmt / clippy -D warnings / 两组 cargo check 全绿；`cargo test --locked`
lib 2492 passed / 0 failed。真机未实测。

---

## 2026-09-03：UI/UX 二轮（Alt+Tab 切换器 / 动画样式 / 通知持久化）

1. **按住式 Alt+Tab MRU 窗口切换器**。默认键位变更：Alt+Tab/Alt+Shift+Tab 从
   loopview（工作区循环）改为 `window_switcher(±1)`；loopview 保留在
   Alt+Page_Up/Page_Down 与三指滑动手势。列表 = `monitor_stack` 的 MRU 序
   （sel_mon 在前，同 launcher 窗口序），过滤 swallowed/hidden/非活跃 tag；
   初始选中 `1 % len`（按一下回前一个窗口）。面板复用 SystemUi 列表
   （新 `ListKind::WindowSwitcher`，行格式复用 `launcher::window_row`），
   零新渲染代码。激活期间键盘被抓取且**全部按键被消费**：Tab/Shift+Tab/上下
   方向键环回移动（wrap，与 expose 的 clamp 语义相反，是有意的），Return
   提交，Escape 取消；释放 Alt/Super/Ctrl 提交（Shift 刻意不算——
   Alt+Shift+Tab 先松 Shift 不该提前提交）；点击面板行=提交该行，点击别处=
   取消。激活瞬间采样修饰键状态（`query_pointer_root`），抓不住的快速点按
   直接提交首选项不卡面板。纯逻辑在 `jwm/features/switcher.rs`（eligibility/
   initial_selection/row/commit_target 均有单测）；提交时重校验窗口仍存活可见，
   失效降级为取消。不含最小化窗口（恢复语义不属于切换手势）。
2. **窗口开关动画样式**。新配置 `behavior.window_animation_style` =
   `scale`（默认，现状不变）/ `fade`（纯透明度）/ `slide`（24px 上滑+淡入，
   `SLIDE_OFFSET_PX`）。纯映射 `compositor_common/window_animation.rs`：
   `window_animation_frame(style, progress, scale_from) -> (scale, alpha, dy)`，
   非法值回退 scale（`validate_choice` 集中校验）。fade/slide 复用现有
   `fade_opacity` 载体（自动贯穿阴影/glow/遮挡判定，不双重叠加），scale 走
   原 `anim_scale` 载体且有往返一致性单测。两端渲染各自四处绘制点接入同一
   frame，观感不漂移。
3. **通知历史跨重启持久化**。`$XDG_DATA_HOME/jwm/notification-history`
   （JSON，`version: 1`，最新 64 条，原子写 0600，损坏/超限/缺失 = 空历史
   启动不崩）。加载在 `FeatureStates::new()`，保存在每次历史变更
   （post/close/clear 三个汇聚点）。`next_id` 入盘防重启后 id 冲突。
   序列化/解析是纯函数（`serialize_history`/`parse_history`），IO 薄壳镜像
   launcher UsageStore 范式。docs/notifications.md 已同步。注意：transient
   hint 目前根本不进 WM（bridge 不转发），持久化内容与内存历史严格一致。

验证：fmt/clippy -D warnings/两组 cargo check 全绿；`cargo test --locked`
lib 2487 passed / 0 failed，集成套件全过。真机端到端未实测（无显示会话）：
Alt 释放提交依赖 keyboard grab 后的 KeyRelease 投递（X11 两后端走 grab mask，
wayland 走键盘焦点），真机验证时优先试这一条链路。

---

## 2026-09-03：UI/UX 交互闭环一轮（tabs / toast / expose）

1. **Tab 条 hover 高亮（两后端）**。hover 状态不进 `Tab`/`TabGroup`
   （避免每个 motion 事件触发全量标题纹理重建），而是照 expose hover 先例放合成器：
   纯命中 `compositor_common::window_tabs::tab_hover_at`（`window_tabs.rs:229`），
   两端 `set_mouse_position` 比较置位，`render_tab_bar` 给 hover 的 inactive 格画半强度
   chip（`ui_theme::TAB_HOVER_ALPHA_SCALE = 0.5`，两端共用同一常量）。
   `set_window_groups` 变化时用缓存鼠标坐标重算 hover，索引越界天然不命中。
2. **Tab 中键关闭窗口**。`Jwm::close_window_tab`（`jwm/window_tabs.rs:198`），
   `input_handler.rs` 的 tab 点击分支按 `MouseButton::from_u8(detail_btn)` 分流：
   Middle → 关窗（走 `window_ops().close_window`，同 killclient），其余维持 focus。
   索引→窗口映射由 `window_tab_hit` 直接查 `tab_group_clients` 的 Vec，无 id 跨边界。
3. **Tab 左键拖拽换序**。WM 侧轻量状态 `tab_drag: Option<TabDragCtl>`
   （`jwm.rs:144`，永不 arm 后端交互，与 `DragCtl` 互斥）；press 记录起点，
   motion 过 `drag_threshold_px` 激活（无实时预览），release 经纯函数
   `plan_tab_reorder`（`window_tabs.rs:325`，同位/组外/越界 clamp 都有回归）在该显示器
   `monitor_clients` 里 remove+insert 后 `arrange`。release 提交块位于
   `event_dispatcher.rs:495` 既有 `apply_drag_snap` 路径之前且处理后即 return；
   打开 system UI 面板时一并清理（`toggles.rs:1196`）。
4. **Toast 悬停暂停 + 左键点击关闭**。`ToastStack` 新增 `set_hovered`/`dismiss`
   （`compositor_common/toast.rs:165,186`）：悬停冻结年龄（unpause 时 created 前移
   结算），点击后 120ms 快速淡出（`TOAST_DISMISS_FADE`）再经 prune 释放纹理，
   dismiss 优先于 hover。两端合成器每帧在 `render_toasts` 记录 `toast_rects`，
   hover 走 `set_mouse_position`，点击经新 API `compositor_click_toast(x, y) -> bool`
   （`api.rs:2346`）；WM 在 `input_handler.rs:2085`（expose 模态分支之后、正常分发
   之前）对左键调用，命中即吞掉，不会 ReplayPointer 给下面重叠的客户端窗口。
5. **Expose 键盘导航**。方向键移动高亮、Return/KP_Enter 聚焦退出、Escape 不变；
   进入时 `compositor_expose_select` 预选当前聚焦窗口（`toggles.rs:2653`），
   直接回车 = 回到原窗口。网格移动是纯函数
   `compositor_common::expose::move_expose_selection`（行列边界 clamp 不 wrap，
   末行不整 Down 不动），cols 公式抽成 `expose_grid_cols` 与 `build_expose_entries`
   单一来源。键盘选中与鼠标 hover 共享 `is_hovered` 通道；X11 侧 hover 语义顺势
   统一为「唯一高亮」（原来条目重叠可多个 hovered）。新 API：
   `compositor_expose_move/_selected/_select`（`api.rs:2388-2396`），
   id 翻译照 `compositor_expose_click` 既有路径。

验证：`cargo fmt --all -- --check`、`cargo test --lib`（2460 passed / 0 failed）、
`cargo clippy --locked --lib --bins --tests --no-deps -- -D warnings`、
`cargo check --locked --all-targets`（默认与 --no-default-features）全绿。
真机端到端未实测（无显示会话），行为验证全部来自纯函数单测。

**已知边界**（可接受，未列待办）：tab 拖拽激活期间无实时预览；toast 点击只关闭
不触发通知动作；跨屏释放 tab 若目标组不含被拖窗口则取消（不跨屏移动窗口）。

---

## 2026-08-22：可靠性、色彩和 CI 闭环

1. 非 D65 显式白点现在经 Bradford CAT 进入目标白点，中性色与 D50↔D65
   往返有纯数学回归；非法/退化 explicit primaries 在 protocol Create 阶段直接失败，
   数学层仍以 identity 二次兜底。
2. 全屏截图会同步预检输出目录和 staging 文件；IPC 成功明确返回
   `{status: "queued", path}`，unsupported/backend/preflight error 则为 `success: false`。
   区域截图回退的真实错误不再被吞掉；KMS 连续请求进入有序队列，PNG 只在完整
   encode/sync 后原子发布，不覆盖已有目标或暴露半写文件。
3. `jwm-tool wayland-status` 在 IPC 与所有兼容查询都失败时非零退出，JSON/
   文本均区分 complete、degraded 与 unavailable。
4. `xcb` 升至 1.7.1，锁文件不再含有受 RUSTSEC-2026-0194/0195 影响的
   build-only `quick-xml` 0.30。
5. WaterLily 移除零引用且会在 headless CI 初始化 GLFW 的 GLMakie，并显式
   声明 `Random`；GTK/Relm bar 补齐 protocol-v14 `ClientIcon` 节点分支。
6. 仓库不再含 Rust `#[ignore]`：toolbar contact sheet 改为纯内存逐格合成断言；
   shared_structures 子进程 helper 由父测试通过 child mode 普通 exact-test 入口分派。

## 2026-08-22：外部帧尾观测十轮加固

1. 外部帧尾 blocker 改为 `LinearTailBlocker` 类型，避免调用点拼写漂移。
2. 每个 blocker 的稳定 wire name 集中在唯一映射中。
3. `ExternalElementColorPlan` 用单一 bitset 保存状态，安全位与清单不再来自两套 bool。
4. cursor/output 命中抽成 half-open 纯几何函数。
5. 负坐标输出、左上包含与右下排除边界有独立回归。
6. 几何运算提升到 i64，i32 极值不再依赖饱和加法；非有限 pointer fail-closed。
7. DPMS-off/soft-disabled 等不参与输出不会贡献 cursor/layer blocker。
8. 跨输出清单只累积不被后一个空输出清除，policy 记录边界只接受类型化 blocker。
9. `ColorDeliveryPolicyDecisionStatus` 可区分 unknown 与 observed-clear，并检查 safe/list、名称和去重一致性。
10. render-decision IPC 同步公开当前 policy 的观测状态、blocker、safe 位与一致性结果，含 legacy/异常 payload 回归。

## 2026-08-22：外部帧尾 schema/IPC 二十轮加固

1. 当前 compositor blocker 的 wire name 改为 API 层唯一常量表。
2. `is_known_linear_tail_blocker_name` 让诊断可识别当前版本名称而不复制匹配表。
3. typed、raw JSON 与反序列化入口共享 32 项清单上限。
4. `LinearTailInventoryObservation` 类型化 unknown/clear/blocked/malformed。
5. `LinearTailInventoryIssue` 为每种失配提供稳定且不含 payload 的类别。
6. `LinearTailInventorySummary` 统一状态、issue 与非敏感计数。
7. 缺失/null 清单稳定归类为 unknown，并保持 schema-v1 兼容。
8. `Some([])` 单独归类为 observed-clear。
9. 非空合法清单单独归类为 observed-blocked。
10. 非数组值归类为 malformed/non-array，而不是与 unknown 混同。
11. 数组中的非字符串归类为 non-string-blocker。
12. 空、超长或非 canonical 名称归类为 invalid-blocker-name。
13. 重复名称归类为 duplicate-blocker。
14. safe 位与空/非空清单冲突归类为 safe-flag-mismatch。
15. raw future schema 若有清单但无 safe 位，归类为 missing-safe-flag。
16. 当前已知 blocker 计数由唯一名称表计算。
17. 合法 future blocker 保持前向兼容，并单独进入 unknown 计数。
18. typed status 与 render-decision IPC 调用同一分类器，不再维护两套一致性逻辑。
19. IPC 新增 state/issue/total/known/unknown 字段，同时保留旧 observation 字段兼容。
20. typed/raw/serde/IPC 回归覆盖 legacy、clear、future、重复、类型错误、safe 失配和超限清单。

---

## TODO: wayland_udev 消除帧尾颜色域缺口并建立可观测性（2026-08-11）

### 进展（2026-09-03）：P0 外部元素颜色计划已完成

`external_elements_color_pipeline_safe` 的总 bool 已拆成逐类计划
（`ExternalElementColorPlan`，`src/backend/udev_kms.rs`）：cursor（主题位图与软件
fallback 同一类）、DnD、session-lock、layer-top、layer-overlay 五类逐输出记录
`Hidden / ExternalAssembly / ImportBlocked` 判定与 stable basis token。计划以实际
可见/可导入为准：未 commit 的 DnD/lock/layer surface tree 不再触发 fallback（修正了
原来的 false-positive）；非参与输出（DPMS-off/soft-disabled）恒 Hidden；导入预检用
`buffer_type` + `dmabuf_texture_formats`，subsurface tree 任一不可导入则该类
`ImportBlocked`。同一份计划驱动 `render_if_needed` 的元素组装（cursor 坐标一次解析、
判定与绘制共用；`rect_overlaps_output` 共享几何）、颜色交付 route 的 blocker 清单与
IPC 诊断，检查与绘制不再维护两套枚举。`TopOrOverlayLayerSurface` 拆成
`top_layer_surface`/`overlay_layer_surface` 两个 wire name，`api.rs` 唯一名称表扩到
8 项并保留 legacy 名 `top_or_overlay_layer_surface` 可识别。render-decision IPC 新增
`color_pipeline.external_elements`（逐类 visible/importable/assembly/blocker/outputs/
basis），typed/raw/serde/IPC 一致性测试同步扩展。route 行为仍收敛 exact-sRGB
fallback；`ImportBlocked` 与 `assembly` 字段是第 3 项 internalize/adapt 的预留接口。
后续从第 3 项继续。

### 进展（2026-08-20）：P0 last-success 诊断已完成

`get_wayland_status`、`get_hdr_status` 和 `get_color_management_status` 现在共享
schema v1 `color_delivery`：`last_policy_decision` 独立记录合成路线选择/阻塞原因，
每个输出的 `last_success` 只在对应 DRM page-flip/vblank 后递增 generation，并带上
queue 时的 policy sequence 与实际逐输出路线。`queue_frame`/render 失败、DPMS 取消和
frame-pending watchdog 都会丢弃 pending 计划，不会覆盖最后成功快照；DPMS 或
disable_head 参与周期变化会使旧快照失效。无成功帧时明确返回 `last_success: null`。
快照报告 route、working space、目标 TF/primaries、HDR metadata/Colorspace signal 和
fallback reason。下面第 1 项已落地，后续从第 2 项继续。

**现状**

`01b41a8` 已建立 normalized linear-sRGB 工作空间、逐输出软件交付区域，以及成对安装/
回滚的 CRTC CTM + GAMMA_LUT 路径。合成器内部窗口、3D overview 和 retained effect
已经在最终输出变换前进入同一个线性工作域。

但 cursor、DnD drag icon、session-lock surface、top/overlay layer surface 仍由 KMS/Smithay
在 compositor texture 之外组装，没有经过 source -> common-linear 转换。只要其中任一元素
可见，`external_elements_color_pipeline_safe` 就会让整帧退回 exact-sRGB；普通桌面的光标
通常位于活动输出上，因此逐输出交付目前主要还是基础设施。若干 encoded-only 帧尾 overlay
和 capture 也仍是独立 blocker。为避免 HDR signal 与实际像素域不一致，
`HDR_OUTPUT_METADATA` enable 继续 fail-closed 拒绝，EDID HDR profile 只作为能力信息。

**原则：HDR enable 是本队列的终点，不是下一个补丁。** 仅适配 cursor/KMS 外部元素仍不足以
安全开放 HDR；absolute luminance、surface-description commit latch、10-bit scanout，以及颜色
属性与匹配 framebuffer 的原子提交都是硬前置。以下里程碑应独立落地，任何未满足条件都保持
exact-sRGB fallback。

**缺口**

1. **[已完成] last-success 交付快照。** IPC 已能区分能力/配置、最近 policy decision 和
   逐输出最后成功呈现，实际报告 global-sRGB、software region、KMS CTM+LUT 或
   direct-scanout 路线，以及 TF、primaries、HDR/Colorspace signal 与 fallback reason。
2. **外部元素没有统一颜色所有权。** 需要把 cursor、DnD、lock、top/overlay 全部
   internalize 到 common-linear compositor pass，或提供数学等价的 per-element adapter；
   不能只修 cursor，否则剩余元素仍会触发同一个 fallback。
3. **几何与 alpha 契约尚未锁定。** internalize/adapt 时必须保留 cursor hotspot、输出
   transform/scale、layer z-order、damage 和 premultiplied alpha；无颜色描述的元素按 sRGB
   ingress，导入失败或描述不受支持时必须退回 exact-sRGB，不能混域继续提交。
4. **帧尾仍有第二套颜色域。** Expose/Peek、tabs、particles、edge glow、HUD、annotation、
   toolbar、toast/OSD、recording overlay 等必须逐类标注为 common-linear-aware，或保留具名
   blocker；capture/readback 要从明确编码的独立 view 派生，不能通过改变物理 scanout route
   来获得截图。
5. **真实 HDR 语义尚不完整。** normalized linear-sRGB 目前没有统一 absolute-luminance/
   working-white 或 tone mapping；非 D65 白点已经 Bradford 适应，surface description 仍尚未与
   对应 `wl_surface.commit` 原子锁存。
6. **KMS 交付还不是 framebuffer 原子事务。** `DEGAMMA/CTM/GAMMA`、connector
   `Colorspace`/`HDR_OUTPUT_METADATA` 与目标 FB 必须同一 TEST_ONLY + atomic commit；还要
   明确要求并验证 HDR scanout 的 10-bit（或更高）format/plane/connector 链。direct scanout
   在未证明 profile passthrough 正确前继续阻断。

**落点与顺序**

1. **P0：[已完成] last-success 诊断** — `src/backend/udev_kms.rs`、
   `src/backend/wayland_udev/backend.rs`、`src/jwm/ipc_handler.rs`
   - 只在 framebuffer 对应 page-flip/vblank 到达后更新 generation、逐输出 route/target、
     tracked signal 和 blocker；失败或 blocked decision 不得覆盖上一份成功快照，独立属性
     transition 则先使旧证据失效，直到替换帧成功。
   - IPC 明确区分 EDID capability、用户 request、attempt 与实际 scanout，不再从配置静态推断
     active HDR；尚无成功快照时返回 null/unknown。
2. **P0：[已完成] KMS 外部元素颜色计划** — `src/backend/udev_kms.rs`
   - 把 `external_elements_color_pipeline_safe` 的总 bool 拆成可诊断的逐类计划，完整覆盖
     cursor（主题与 fallback）、DnD、lock、top/overlay 及各自 subsurface tree；计划必须以
     实际可见/可导入元素为准。
   - 让同一份计划驱动 element assembly、颜色交付 route 与 blocker reason，避免检查与绘制
     两套枚举再次漂移。
3. **P0：internalize/adapt** — `src/backend/wayland_udev/backend.rs` 与
   `compositor/{render,mod}.rs`
   - 优先把上述元素按正确 z-order 绘入 FP16 common-linear target，再执行现有逐输出
     matrix + OETF；保留 exact-sRGB fallback 作为每帧 fail-closed 路径。
   - 复用 `color_management.rs` / `color_pipeline.rs` 的 sRGB ingress、矩阵布局和 transfer
     规则，不为 cursor/layer 复制另一套 GLSL 传递函数。
4. **P0：清理其余 linear-tail blocker** — `compositor/{damage,render,expose}.rs`
   - 建立帧尾 domain table，让每一类 overlay 要么在 final delivery 前绘制，要么有显式颜色
     adapter；capture/recording 使用独立、目标明确的 view，不再反向约束物理输出 route。
5. **P1：补齐颜色语义** — `color_management.rs`、`color_pipeline.rs` 与 surface commit 路径
   - 非 D65 Bradford CAT 已实现并测试。剩余：定义 working white/absolute
     luminance、SDR/PQ/HLG 标尺与 tone-map policy；将 image-description
     pending/current 双缓冲，只在匹配 surface commit 生效。
6. **P1：KMS 原子交付与位深** — `src/backend/udev_kms.rs`
   - 将 plane FB、CRTC color stages 与 connector signalling 合并为同一受控 atomic request；
     跨 DRM device 或 10-bit 链路不完整时继续软件 SDR，不宣称 hardware HDR active。
7. **P2：开放真实 HDR enable** — `src/backend/udev_kms.rs` 与 `src/jwm/ipc_handler.rs`
   - 只有工作域、全部可见 tail element、capture、atomic KMS 和位深门槛同时满足时，才提交
     `Colorspace` + HDR metadata + 匹配 FB；使 enable/disable、DPMS、gamma-control、
     hotplug/reinit 和 compositor runtime toggle 都保持可回滚的一致状态。
8. **贯穿测试** — `src/backend/wayland_udev/compositor/headless_render.rs` 及 KMS 纯策略测试
   - 增加外部元素 source-sRGB -> common-linear -> PQ/HLG/sRGB output 的像素 oracle，覆盖
     半透明边缘、cursor hotspot、跨输出移动、DnD、lock、top/overlay 和导入失败。
   - 覆盖 last-success 快照、route 切换、capture 不改变 scanout、10-bit format 协商、属性/
     pageflip 提交失败、DPMS/hotplug/reinit，以及“旧 framebuffer + 新属性”不得被提交。

**验收目标**

- IPC 的 active route/signal 只来自最后一次成功呈现；失败 attempt 不会产生 false-active。
- 普通 cursor、DnD、lock、top/overlay 可见时不再天然触发 global-sRGB fallback；任何未适配
  或导入失败的元素仍会稳定、可诊断地退回 exact-sRGB。
- SDR 画面与当前路径像素一致；PQ/HLG/广色域输出中外部元素只做一次 OETF/色域变换，
  alpha 混合、跨输出边界和 mixed-output region 都有严格 EGL 像素回归。
- capture/recording 与每类帧尾 overlay 都有 domain 契约；启用它们不造成双重编码，也不会
  为获取截图而切换物理输出 signal。
- HDR metadata 只有在 absolute-luminance、commit latch、10-bit 链路，以及 framebuffer 与
  完整颜色属性的同一受控提交都验证成功后才标记 active；失败、关闭、VT/KMS reinit 后不会
  遗留 connector/CRTC 颜色状态。
- `cargo fmt --all -- --check`、六组 backend feature check、严格 surfaceless EGL 全套和 KMS
  状态机测试全部通过。

**P0（里程碑 1–4）的非目标**

- 开放 HDR signalling；在 P1/P2 完成前 enable 继续明确拒绝。
- HDR/SDR absolute-luminance、tone mapping、working-white 和 surface-description
  commit latch；这些属于 P1，不能用 P0 的 normalized workspace 冒充完成。
- negative-origin、non-unit scale、rotated 或冲突 overlap 输出拓扑的逐输出交付。

---

## DONE: wayland_udev 补齐 attention_animation（2026-08-07）

`attention_animation` 现在真正控制 Wayland 紧急边框，颜色、脉动周期与
2 px 最小宽度与 X11 共用同一纯策略。渲染循环在 urgent 状态下持续
驱动脉动；即使 `border_enabled = false`，KMS direct scanout 也会退回合成，
不再绕过这条状态信号。纯策略测试覆盖 alpha 节奏、窗口 fade、
最小宽度和 direct-scanout 门禁。最终状态传播也已闭环：ICCCM `WM_HINTS`
与 EWMH `_NET_WM_STATE_DEMANDS_ATTENTION` 都同步 WMClient 和 compositor；
Wayland 在纹理状态尚未创建时暂存 initial urgency，并在首建时消费、销毁时清理。
下文保留修复前的完整审计记录。

**修复前现状（2026-08-02 核对）**

X11 合成器完整实现了紧急窗口的脉动边框；wayland_udev 只有一条**硬编码的静态红边**，两个配置项读进来了但渲染路径从没读过。

- `behavior.attention_animation`（bool，默认 false）→ `wayland_udev/compositor/config.rs:160`
  存进 `attention_animation_enabled`（`mod.rs:720`），**全仓库没有第二处引用**。
- `behavior.attention_color`（默认 `[1.0, 0.4, 0.1, 1.0]`）→ `config.rs:237`
  存进 `attention_color`（`mod.rs:997`），同样**没有第二处引用**。
- 实际绘制在 `wayland_udev/compositor/render.rs:2023-2024`：
  ```rust
  } else if wt.is_urgent {
      [1.0f32, 0.2, 0.2, 0.9 * fade]
  ```

**四个缺口**（对照 X11 `x11/compositor/render.rs:4478-4547`）

1. **开关无效** —— urgent 边框恒亮，`attention_animation = false` 关不掉它。
2. **颜色写死** —— 忽略 `attention_color`，X11 是橙 `[1.0,0.4,0.1]`，wayland 是红 `[1.0,0.2,0.2]`，两个后端观感不一致。
3. **没有脉动** —— X11 用 `(elapsed * 4.0).sin() * 0.5 + 0.5` 调制 alpha（周期约 1.57 s，`render.rs:4494`），wayland 是恒定 alpha 0.9。
4. **被 `border_enabled` 吞掉** —— wayland 的整个边框块在 `if self.border_enabled` 里（`render.rs:1955`），且不给 urgent 加宽；X11 靠 `has_special_border` 绕过门禁并强制 `max(2.0)` 宽度（`render.rs:4478, 4506-4514`），所以关了边框紧急信号依然可见。这是有意设计，紧急信号不该被边框设置吞掉。

**原计划落点（已实现）**

- 主改：`wayland_udev/compositor/render.rs` 第 10 段 "Draw borders"（1952-2060）。
  - `border_color` 的 `else if wt.is_urgent` 分支改成读 `attention_animation_enabled` + `attention_color` + 同一条 sin 脉动公式；关掉开关时该分支应整体跳过，退回普通聚焦/非聚焦色。
  - `border_width` 补 urgent 的 `max(2.0)`。
  - `if self.border_enabled` 这个外层门禁要让 urgent 窗口穿过去（参考 X11 的 `has_special_border`），否则第 4 点修不掉。注意 `1975-1978` 的 `if !is_focused && !wt.is_urgent { continue; }` 已经放行了 urgent，只差外层。
  - `use_gradient`（`render.rs:2050-2051`）已经排除了 `is_urgent`，无需改动。
- 无 tick 保活：`render.rs:953-958` 的 `any_animating` 要加一项「开关开着且存在 urgent 窗口」，否则脉动只走一帧就静止。X11 对应 `x11/compositor/config.rs:165-171` + `render.rs:2841`。
  ⚠️ 代价一并继承：只要有 urgent 窗口没人理，合成器就持续满帧重绘。X11 已经是这个行为，两边保持一致即可，但值得在 commit message 里点明。
- `render.rs:596-604` 已经把 urgent 窗口塞进 dirty box（注释就是为这条边框写的），改完之后那段防护才真正有意义，不用动。

**验收目标**

- `attention_animation = false` 时 urgent 窗口只有普通边框；`true` 时呈 `attention_color` 呼吸。
- `border_enabled = false` + urgent → 仍有 2px 脉动边框。
- 两个后端并排跑同一份 config，颜色与节奏肉眼一致。
- urgent 触发源：`WM_HINTS` urgency（`jwm/window_state.rs:208-235`）或 `_NET_WM_STATE_DEMANDS_ATTENTION`（`jwm/event_dispatcher.rs:818-828`）；聚焦即自动清除（`jwm/focus.rs:57-62`）。测试时注意勿扰模式会直接抹掉未聚焦客户端的 urgency。

---

## window_tabs：已完成的重构与剩余项

**2026-08-02 做完的（全部未提交）**

起点是 X11 下 tab bar 完全不显示：`compositor_set_window_groups` 是委托宏里唯一没做
id 翻译的按窗口调用，WM handle 被当成 xid 塞进 `WindowTab.x11_win`，而查找端拿的是
`render_frame` 翻译过的真 xid，两者永远不等。先在委托处补了翻译，随后重构把这类
bug 整个消掉了 —— 现在没有任何窗口 id 跨过这条边界。

新架构照搬 `layout_strip`（胶片选择器）的范式，三方共用一个纯几何模块
`compositor_common::window_tabs`：

- **WM 预留 + 拥有语义**：`jwm::window_tabs` 里 `tab_group_clients` 是唯一判据
  （开关开着、同屏 ≥2 个可见平铺窗口、没有全屏窗口）；`monitor_work_area` 现在
  = `monitor_work_area_untabbed` 减去顶部 `tab_bar_height`，所有布局/浮动落位/贴边
  自动生效，bar 落在被让出来的那条带里，不再压状态栏。
- **合成器只收矩形和标题**：`TabGroup { bar: Rect, tabs: Vec<Tab { title, active }> }`。
  没有窗口 id 可翻译错，两个后端的绘制路径也因此统一。
- **点击**：WM 用同一个 `window_tabs::tab_at` 命中测试
  （`Jwm::click_window_tab`，接在 `on_button_press_internal` 的背景点击分支里），
  focus + restack 走和点击窗口完全相同的路径。
- **标题纹理缓存**：以前每帧、每个标签都在 CPU 光栅化标题再 create/upload/delete
  一张纹理；现在只在 `set_window_groups` 检测到变化时重建（`refresh_tab_titles`），
  静止桌面上每帧零上传。X11 的纹理在合成器 teardown 里一并删除。

已验证（Xephyr :20，xcb + GLX）：2 窗口时窗口整体下移 28px、全宽两格；3 窗口时三等分；
点左/中/右格分别切到对应窗口；点已激活的格子不动栈序；关到只剩 1 个窗口时预留自动
收回。`cargo test --lib` 1238 passed。

**剩余项**（2026-09-03 四轮后核对：全部关闭）

1. ~~两个后端的标题样式不一致~~ **已关闭**：overview 标签在三轮统一迁入
   `compositor_font` + ui_theme 药丸 chip，两端同源同样式，位图字体文件已删。
2. ~~非 ASCII 标题显示为 `?` 或空格。~~ **已关闭**：tab 标题本就走
   `compositor_font` 真字体栈，overview 标签在 2026-09-03 三轮迁入后，
   用户可见文字全部支持 CJK/emoji 回退；仅剩无 fontconfig 环境的兜底位图表。
3. ~~没有 hover 高亮、没有中键关闭、不能拖拽换序。~~ **已完成**，见 2026-09-03
   「UI/UX 交互闭环一轮」第 1–3 条。
4. ~~`compositor_zoom_to_fit(Option<u32>)` 未翻译裸 id~~ **已关闭**：四轮改为
   `Option<WindowId>` 并在两端委托处翻译。注意 wayland render 端只存不用，
   将来接线若需 wayland 实际缩放要补渲染路径。
