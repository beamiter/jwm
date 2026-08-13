# Handoff

滚动记录待办：每条写清楚「现状 / 缺口 / 落点」，让下一次接手的人不必重新考古。

---

## TODO: wayland_udev 消除帧尾颜色域缺口并建立可观测性（2026-08-11）

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

1. **没有 last-success 交付快照。** IPC 当前只能报告能力/配置，不能回答最后一次成功呈现
   实际走了 global-sRGB、software region 还是 KMS CTM+LUT，也不能可靠报告逐输出 TF、
   primaries、HDR/Colorspace signal 与 fallback reason。
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
   working-white、tone mapping 或非 D65 chromatic adaptation；surface description 也尚未与
   对应 `wl_surface.commit` 原子锁存。
6. **KMS 交付还不是 framebuffer 原子事务。** `DEGAMMA/CTM/GAMMA`、connector
   `Colorspace`/`HDR_OUTPUT_METADATA` 与目标 FB 必须同一 TEST_ONLY + atomic commit；还要
   明确要求并验证 HDR scanout 的 10-bit（或更高）format/plane/connector 链。direct scanout
   在未证明 profile passthrough 正确前继续阻断。

**落点与顺序**

1. **P0：last-success 诊断** — `src/backend/udev_kms.rs`、
   `src/backend/wayland_udev/backend.rs`、`src/jwm/ipc_handler.rs`
   - 只在 framebuffer/属性成功提交并进入可呈现状态后更新 generation、逐输出 route/target、
     active signal 和 blocker；失败或 blocked attempt 不得覆盖上一份成功快照。
   - IPC 明确区分 EDID capability、用户 request、attempt 与实际 scanout，不再从配置静态推断
     active HDR；尚无成功快照时返回 null/unknown。
2. **P0：KMS 外部元素颜色计划** — `src/backend/udev_kms.rs`
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
   - 定义 working white/absolute luminance、SDR/PQ/HLG 标尺与 tone-map policy；实现并测试
     非 D65 CAT。将 image-description pending/current 双缓冲，只在匹配 surface commit 生效。
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
- HDR/SDR absolute-luminance、tone mapping、working-white、非 D65 CAT 和 surface-description
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

**剩余项**

1. **两个后端的标题样式不一致。** X11 的 `render_title_to_pixels`（`x11/compositor/overview.rs`）
   给标题画了一个深色药丸底 + 2x 放大；wayland 的同名函数（`wayland_udev/compositor/overview.rs`）
   是 1x 白字无底。两个函数各有自己的字模表，还都被各自的 overview 用着，统一它们
   要动 overview，所以这次没碰。
2. **非 ASCII 标题显示为 `?` 或空格。** 两边都是 6x10 ASCII 位图字体，中文标题会整排问号
   （X11）或空格（wayland）。要修就得引入真正的字体栈，是另一个量级的事。
3. **没有 hover 高亮、没有中键关闭、不能拖拽换序。** 命中测试已经在 WM 手里
   （`window_tab_at`），加这些只需要在 `on_motion_notify` / 按钮分支上接线。
4. **`compositor_zoom_to_fit(Option<u32>)`**（`compositor_delegation.rs`）仍是原来那种
   未翻译的裸 u32 形状，`zoom_to_fit` 用 `self.windows.get(&win)` 按 xid 查表。
   目前全仓库没有调用者所以没暴露；接线之前记得先翻译，否则就是刚修掉的那个 bug。
