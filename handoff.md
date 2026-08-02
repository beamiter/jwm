# Handoff

滚动记录待办：每条写清楚「现状 / 缺口 / 落点」，让下一次接手的人不必重新考古。

---

## TODO: wayland_udev 补齐 attention_animation

**现状（2026-08-02 核对）**

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

**落点**

- 主改：`wayland_udev/compositor/render.rs` 第 10 段 "Draw borders"（1952-2060）。
  - `border_color` 的 `else if wt.is_urgent` 分支改成读 `attention_animation_enabled` + `attention_color` + 同一条 sin 脉动公式；关掉开关时该分支应整体跳过，退回普通聚焦/非聚焦色。
  - `border_width` 补 urgent 的 `max(2.0)`。
  - `if self.border_enabled` 这个外层门禁要让 urgent 窗口穿过去（参考 X11 的 `has_special_border`），否则第 4 点修不掉。注意 `1975-1978` 的 `if !is_focused && !wt.is_urgent { continue; }` 已经放行了 urgent，只差外层。
  - `use_gradient`（`render.rs:2050-2051`）已经排除了 `is_urgent`，无需改动。
- 无 tick 保活：`render.rs:953-958` 的 `any_animating` 要加一项「开关开着且存在 urgent 窗口」，否则脉动只走一帧就静止。X11 对应 `x11/compositor/config.rs:165-171` + `render.rs:2841`。
  ⚠️ 代价一并继承：只要有 urgent 窗口没人理，合成器就持续满帧重绘。X11 已经是这个行为，两边保持一致即可，但值得在 commit message 里点明。
- `render.rs:596-604` 已经把 urgent 窗口塞进 dirty box（注释就是为这条边框写的），改完之后那段防护才真正有意义，不用动。

**验收**

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
