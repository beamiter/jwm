# Handoff

滚动记录待办：每条写清楚「现状 / 缺口 / 落点」，让下一次接手的人不必重新考古。

---

## 2026-09-04：十二轮收口之二（HDR P2：条件式 enable，逐帧和解）

**enable 不再是硬拒，但也不是一次性开关——它是一个控制回路。**

1. **唯一判据 `hdr_enable_refusal`**（纯函数，风格照抄
   `hdr_scanout_chain_gap`）：`HdrEnableEvidence` 全是 plain data，11 个具名
   refusal，优先级固定为「硬件 → 配置 → 本帧内容」，所以一台永远不行的输出
   报的理由与屏幕上放什么无关。**IPC 请求与逐帧和解调用同一个函数**——这正是
   tail-domain 表当初给 overlay 立下的规矩：门禁与帧循环不能各写一份。
   `set_hdr_metadata_for_output` 内部仍自己重跑 chain 校验，本策略是**追加**
   门禁而非替代。
2. **意图与状态分开**。`output_hdr_metadata_active` 是 commit 之后的事实，不是
   意图；没有锁存的话，第一个 toast 让帧尾不安全、HDR 一撤就永远回不来（除非
   用户再发一次命令）。新增 `KmsOutputState.hdr_requested` 锁存意图，
   `hdr_signalling_action(requested, refused, active)` 每帧给出
   Hold/Assert/Withdraw。IPC 命令只在**等不来的**拒绝上直接失败
   （`hdr_enable_refusal_is_permanent`：SDR 面板、scanout 链缺口、开关没开），
   一时性的（toast 正在屏上）不失败——锁存就是用来跨过它的。
3. **撤回面从 1 条扩到 11 条**。原来只有 `!scene_linear_active` 会撤，于是
   toast / session lock / 导入失败的 cursor / gamma-control 接管 /
   unsupported topology 全都会让 PQ+BT.2020 的信号盖在 encoded sRGB 的画面上
   ——那是整屏级色度错误，不是细微偏差。
4. **首切片只走 software 路由**，而且**请求会反过来选路**。硬件 CRTC pair 把
   working-linear 写进 unorm 输出 FBO、之后才在 GAMMA_LUT 里过 OETF，>1.0 在
   写入时就被裁掉；software 路由在 encode shader 里**先**过 OETF，PQ 编码后的
   值落在 [0,1] 内、headroom 完整。所以 `hw_pair_target` 在任一参与输出带
   `hdr_requested` 时被抑制（pair 本就是全组同进退，代价是整组转 software）。
   **这一步是必需的，不是优化**：hw pair 只要 CRTC 有 GAMMA_LUT+CTM 就优先选
   中，不抑制的话 enable 会在恰恰能做 HDR 的硬件上被永远拒绝——功能看起来实现
   了、实际一次都不会触发。`HardwareLutRouteClipsHdrHeadroom` 因此退居兜底，
   选路子句本身由 const 源码断言钉住（同
   `render_hot_path_reuses_cached_output_names` 的做法）。
5. **顺带修一个潜伏 bug**：`attach_edid_caps_to_outputs` 用
   `insert_if_missing_threadsafe`，写一次就不再更新；查表按 output **名字**，
   而 `Output` 对象在同一 connector 上热插拔会复用。以前这只影响宣告给客户端的
   image description 大小，现在这份 caps 要去铸 32 字节 CTA-861.3 blob——把旧
   面板的峰值亮度和基色告诉新面板，正是本队列存在的意义所要防的那类错误。改成
   `Mutex<Option<..>>` 槽位并**替换**，读取统一走
   `output_edid_hdr_capabilities`。
6. **IPC 停止硬编码**：`hdr_active` 改为 last-success 观测（不再恒 false），
   `hdr_signalling_enable_available` 由逐输出门禁推导（**没有门禁上报 ≠ 可用**，
   X11/headless 恒 false，有专门断言钉住），固定 blocker 字符串换成
   `hdr_enable_refusals` 逐输出真实理由，`limitations` 删掉三条早已被代码推翻
   的（absolute luminance / commit latch / atomic KMS）。逐输出
   `selected_transfer_function`/`selected_primaries` 原来是字面
   `srgb_params()`，切换成功的输出也照报 sRGB——那让整个逐输出策略无法用来
   验证 enable；现在与 `params_for_output` 同源。`colorspace_signal` 从
   `hdr_metadata_unspecified_colorspace` 改为 `bt2020_rgb`：请求里一直带的就是
   BT2020_RGB，报「unspecified」让「显示器被告知了什么」这唯一的证据失去意义。
7. **DPMS/soft-disable 的撤回不发 commit**：连接器属性属于一台已经关掉的显示器，
   那个 commit 会不会被接受是驱动相关的，而失败会把 `delivery_blocked` 打到**整
   个 delivery group**——关掉一台副屏就能卡住其余所有输出的呈现。所以只清掉
   tracked 标志（不呈现的输出本就不该被声称 active），亮回来时再发一次真 commit。
   其余所有撤回照常 commit：那些输出正在呈现，把 BT.2020 盖在 sRGB 像素上正是本
   队列要防的事。
8. **新增 docs/hdr.md** 并接进 README 索引（本仓库此前没有任何 color/HDR/
   tearing/VRR 的用户文档）。

已知限制（写进 docs/hdr.md 的 Limitations，不冒充完成）：**无真 HDR 显示器
实测**——内核会接受一个语法合法但语义未经校验的 blob，所以「合成器绝不在 sRGB
像素上打 HDR 信号」是被证明了的，「面板把这份 metadata 解读成预期的样子」没有。
硬件 LUT 路由的 FP16 scanout FB 与扩展 LUT 键、FB 与颜色属性的单 ioctl 配对、
`linear_tail_safe` 的逐输出化，均为显式非目标。

验证：fmt / clippy -D warnings / `cargo check --locked --all-targets` /
`--no-default-features` 及 7 组 backend feature profile 全绿；
`cargo test --locked` lib 2685 passed / 0 failed（上一条目 2675 + 新增 10：
udev_kms HDR 门禁/控制回路/atomic plan/选路/暗输出 8、ipc_handler 逐输出
profile 1、序列化形状 1）。

**自查抓到的一个 bug（已修，值得记一笔）**：逐输出 `hdr_signalled` 查找一度用
`entry["name"]` 匹配，而 `api::ColorDeliveryOutputStatus` 的字段是
`output_name`。这种错法不会报错，只会**永远静默返回 false**——切换成功的输出照
旧报 sRGB，正好抵消第 6 条要修的东西。现在两处查找合并成 `presented_with_hdr`，
并由 `presented_with_hdr_reads_the_real_serialized_shape` 用 **serde 真实序列化**
的值钉住（手写 fixture 会把同一个错误抄进测试）。

---

## 2026-09-04：十二轮收口之一（tearing hint 消费 / VRR clobber 修复）

**先说结论：真正的 async page flip 这一轮不做，而且不是「没做完」——是做不了，
本轮把「做不了」变成可诊断的具名 blocker。**

**为什么做不了。** 钉住的 smithay rev（`e76f1af`）在
`DrmOutput → DrmCompositor → DrmSurface` 整条链上没有任何 async flip 旋钮：
`AtomicDrmSurface::page_flip`（`surface/atomic.rs:867-903`）把 commit flag 写死
成 `PAGE_FLIP_EVENT | NONBLOCK`，全 `src/backend/drm/` 里 grep 不到一个
`ASYNC`；`DrmCompositor::submit`/`PreparedFrame`/`QueuedFrame` 全是私有，
`RenderFrameResult` 也不交出 framebuffer handle，所以绕过 `queue_frame` 自己发
flip 同样不可行。要做就得 fork smithay + `[patch]`（Cargo.toml 目前没有
`[patch]` 段），那是供应链决定不是代码决定，**不在本轮单方面做**。

1. **hint 按协议双缓冲**（`tearing_control.rs` 重写）。协议原文：
   `set_presentation_hint` 「will be applied on the next wl_surface.commit」，
   对象 destroy 的回退 vsync 同样在下次 commit 生效。原实现在 request handler
   里直接改 map，等于一个 hint 可能描述的不是它随行的那块 buffer——正是
   `0477ddf` 给 image description 修过的同一个坑。现在 request 只 stage，
   `CompositorHandler::commit` 落 latch（就在 `commit_surface_description`
   旁边），下游只读 committed 半边。map 操作全部对 key 泛型，因此整套状态机不需
   要 live display 造 `ObjectId` 就能单测。
2. **修 hint 泄漏**（真 bug）。协议允许「销毁 wl_surface 但保留 control
   对象」（用词是 should not must），而原来只在 *对象* 死时清表，
   `CompositorHandler::destroyed` 完全没碰 `tearing_hints`。条目会活过
   surface，键是一个 server 之后可能重新发给别人的 ObjectId——文件自己的注释
   （旧 `:142`）正好点了这个危险，却只防住了另一个方向。
3. **补 `tearing_control_exists` 协议错误**。同一 surface 两个 control 对象共享
   一个 hint，销毁第二个会静默清掉第一个的 Async 请求，撕裂与否变成「客户端最
   后销毁了哪个对象」的函数。
4. **修 VRR clobber**（另一个真 bug，本机无 VRR 显示器但可从 smithay 源码证明）。
   jwm 原来在 output init 无条件裸写 CRTC `VRR_ENABLED = 1`
   （旧 `udev_kms.rs:5738`），`set_vrr_for_output` 走同一条裸写路径。但 smithay
   的 `AtomicRequest::set_crtc`（`atomic.rs:1388-1413`）在**每一次** atomic
   request 里都按自己缓存的 `vrr` 重新写这个属性，`page_flip`
   （`atomic.rs:882`）也传 `self.state.vrr`——所以裸写在下一帧就被撤销，
   IPC 却报成功。两条路径现在都走 `DrmCompositor::use_vrr`。
   **必须 gate 在 `VrrSupport::Supported`**：`use_vrr`
   （`atomic.rs:622-685`）在非快路径上只做 `ALLOW_MODESET | TEST_ONLY` 然后
   *只*更新 `pending.vrr`，`current.vrr` 不动 → `commit_pending()` 变真 →
   `DrmCompositor::submit`（`compositor/mod.rs:2548`）下一帧改走 `commit()`
   分支。`RequiresModeset` 因此被显式拒绝而不是「试试看」。
5. **逐输出 presentation 策略**（纯函数 `presentation_plan` +
   `PresentationEvidence`/`PresentationBlocker`，就放在
   `frame_flags_for_color_delivery` 旁边，风格照抄 `hdr_scanout_chain_gap`）。
   VRR 跟内容走（单个 mapped 全屏窗口独占输出时开，合成桌面上关——静态画面上
   开 VRR 会让部分面板闪），tearing 跟客户端请求走，两者读同一份证据、其中
   「一个全屏客户端独占输出」直接复用 direct-scanout 的判定，三个决策因此不会
   各自漂移。`commit_pending` 经
   `with_compositor(|c| c.surface().commit_pending())` 进证据（第 4 条说明了
   为什么它必须在里面）；`DRM_CAP_ATOMIC_ASYNC_PAGE_FLIP` 在 init 探一次并缓存
   ——驱动不支持时内核直接拒绝 async commit，而渲染循环把 queue 失败当设备故障
   去 `activate(false)`，无条件尝试会让这条恢复路径每帧空转。
6. **IPC 说真话，schema 升 v2**。`render_decisions.tearing.active` 原来字面就是
   `hint_count > 0`——把「有人请求」当成「发生了撕裂」，而 jwm 根本不撕，于是
   它一直在为一件从未发生的事报 `active: true`。v2 把需求挪到
   `client_demand`，`active` 只表示真发生，`reason` 给具名 blocker，
   `tearing.outputs` 与新的 `vrr` 块逐输出报告；`get_tearing_hints` 同样加逐
   输出行。语义变了就必须升版本并同步下游：`tools/jwm_tool.rs` 的三处读取点
   （render_decisions 打印、`vrr-tearing` 证据串、两处 tearing 打印）一并改，
   否则诊断工具会静默说谎。
7. **顺带订正两处过期文档**：`docs/compatibility.md` 和 `config.rs` 都声称
   Wayland/KMS「设置 CRTC VRR_ENABLED 属性」——按第 4 条，那个设置活不过一帧。

已知限制：**无 VRR 显示器、无 async-flip 能力驱动可实测**，本轮全部行为验证来
自纯策略单测（blocker 优先级全表、VRR 与 tearing 的独立判据、wire name 唯一性）
与 latch 状态机单测。async flip 的落点已经备好：`SUBMISSION_SUPPORTS_ASYNC_FLIP`
一个常量翻成 true，策略就活，驱动能力探测已经在门前。

验证：fmt / clippy -D warnings / `cargo check --locked --all-targets` /
`--no-default-features` 及 7 组 backend feature profile 全绿；
`cargo test --locked` lib 2675 passed / 0 failed（上一条目 2665 + 新增 10：
tearing_control latch 5、udev_kms 策略 4、ipc_handler tearing v2 1）。

---

## 2026-09-04：十二轮补充（蓝牙 v2 之三：入站授权，用户手动开窗）

v1 记录里 v2 候选的最后一项。**结论先说：做成「用户显式开一个 60s 窗口」，
不做常驻默认 agent。**

**为什么不常驻。** 常驻 agent 要么塞进长期 `jwm-bridge`（它是 session bus 上被
D-Bus 激活的 `org.freedesktop.Notifications`，从不开 system bus，且全仓库唯一
的 spawn 点是 `toggles.rs` 的配对路径——安全相关的 prompt 却要等到本 session
第一条通知才存在，可用性不确定），要么单独常驻进程；两者都把「一台指定设备、
≤90s」变成「任何设备、永远」，且 cookie 从「一次会话」退化成长期进程秘密或者
反向由 helper 铸造——正好推翻 `pairing.rs:236` 与 `main.rs:41` 写死的信任方向。
另外 `RequestDefaultAgent` 是后写者赢：常驻会在整个 session 里把 default 从
blueman/gnome-bluetooth 手里抢走，而对方稍后启动又抢回去，我们的 prompt 从此
静默且无信号。

1. **落点（helper）**：新 `jwm-bridge accept`，cookie 仍走环境变量。自愈查询用
   新的 `inbound_session_matches`——**除 cookie 外还校验 `kind == "inbound"`**，
   否则一个 inbound agent 可能被挂到 jwm 为「配对一台指定设备」开的会话上，那
   正是把窄窗口变成常开门的路径。agent 服务在
   `/org/jwm/inbound_agent`（与 `AGENT_PATH` 分开只为日志可读），
   RegisterAgent + RequestDefaultAgent（入站请求只发给 default agent，不抢就等
   于注册了永远收不到），然后**不调用 `Pair`**，只等回调。
2. **必须开 Pairable/Discoverable**：本机实测两者默认都是 false
   （`busctl get-property … Adapter1 Discoverable Pairable` → `b false` / `b
   false`），不开的话整条功能是「已武装、逻辑正确、完全无法触达」。
   `AdapterExposure` 记下开窗前的原值，收摊时**恢复**而不是清零。
3. **目标晚绑定，不是不绑**：`Shared.target` 变成 `Mutex<Option<String>>`，
   `bind_target` 第一次回调钉住、之后与旧逻辑逐字相同；jwm 侧对称地
   `PairingSession::inbound` + `pin_address`，`matches()` 在未钉住时只接受
   *合法地址*，钉住后恢复严格相等。配对 helper 仍恒拒入站
   （`Shared.accepts_inbound = false`），有专门集成测试钉住。
4. **prompt 泛化**：`PairingPrompt::Authorize { service: Option<String> }`
   （None=RequestAuthorization，Some=AuthorizeService）、
   `PairingPhase::AwaitingAuthorization`、`PromptKind::Authorize`。
   UUID 经 `service_label` 查表转成人能判断的 profile 名（认不出就原样显示——
   编一个名字比显示 UUID 更糟）；service 与 device_name 都按既有规矩定界并拒
   控制字符（远端可控字符串要落到模态面板上）。y/n 走
   `PromptKind::is_yes_no` 与 Confirm 合并成一条分支，`answer_bluetooth_confirm`
   的 phase 守卫同时认两种，其余一律 return。
5. **窗口寿命 60s**（`INBOUND_WINDOW`），比配对的 95s 短：它是投机开的，且整个
   生命期里控制器都是可发现的。`session_timed_out`/`next_timeout_in` 改走
   `lifetime()`；关面板/hand-over/Esc/scrim 全部沿用既有 `cancel_bluetooth_pairing`
   汇合点，窗口不会活过面板。同一时刻仍只允许一个会话（任一方向）。

已知限制与非目标：**未实测真机入站**（本机有 hci0 但没有会主动来配对的外设），
覆盖来自私有 dbus-daemon + 假 org.bluez 的 3 个新集成测试（允许→bluez 收到成功、
拒绝→bluez 收到错误、第二台设备被拒、配对 agent 恒拒入站）。锁屏下的入站没有单
独处理：`prompt_bluetooth_pairing` 本就只在蓝牙 picker 在屏时渲染，而开窗必须是
用户在 picker 里按 `a`，所以「锁屏时收到入站请求」根本不可达。
「记住这次授权」（Trusted 之外的按服务持久化）不做，留 v2.1。

验证：fmt / clippy -D warnings / `cargo check --locked --all-targets` /
`--no-default-features` 及 7 组 backend feature profile 全绿；
`cargo test --locked` lib 2665 passed / 0 failed（上一条目 2659 + 新增 6：
pairing 5、system_ui 1）；bridge 55 passed / 0 failed（上一条目 49 + 新增 6：
纯逻辑 3、集成 3）。

---

## 2026-09-04：十二轮（蓝牙 v2 之二：配对后自动连接 / D-Bus 发现）

v1 记录里列的 v2 候选三项，本轮做掉后两项（入站授权见下一条目）。

1. **配对后自动连接 + Trusted**（`bridge/src/bluez.rs`）。`Device1.Pair` 返回后，
   同一个已解析的 device path 上先 `Properties.Set(Device1.Trusted = true)`
   （本地簿记，不会卡在射频上；没有它，配对好的键盘重启后自己回连时会因为
   没有 agent 授权服务而永远打不出字），再 `Device1.Connect`。两步都不能把
   成功的配对变成失败：`done_args` 多一个 `connected` 字段（`ok=false` 时
   恒 null——失败的配对没有东西可连），jwm 侧 `DoneCommand.connected:
   Option<bool>` + `pairing::paired_message` 给出三种状态行
   （`Connected X` / `Paired X — not connected` / `Paired X`）。
   **时钟不加长，只借用**：`connect_budget(elapsed)` 从 90s 会话墙钟里扣掉
   已用时间和 3s 上报余量，取 `min(剩余, 15s)`，不足 1s 就直接跳过并如实
   上报——因为 jwm 的 `SESSION_TIMEOUT` 是 95s，超时后 `done` 会被拒收，
   面板什么都不会显示。
2. **发现改走 D-Bus**（新 `jwm-bridge discover [seconds]`）。
   `Adapter1.StartDiscovery` + 一次 `GetManagedObjects` 就同时拿到
   name/Alias、Paired、Connected 和 RSSI，取代「`bluetoothctl devices` +
   每设备一个 `bluetoothctl info` 子进程」的扇出（64 设备上限 × 10s 超时
   = 最坏 640s 挂在一个 worker 线程上）。`discover 0` = 只列不扫，`r` 和
   面板首帧用它。**对象树必须在 StopDiscovery 之前读**：bluez 只在还听得见
   设备时才挂 RSSI，扫描停掉之后再读就正好丢掉这一个字段（本机实测：
   停后 33 个设备全 null，停前全部有值）。适配器选择偏好 Powered，同 Powered
   按 path 排序保证多次刷新选同一个控制器。jwm 侧 `parse_bridge_devices`
   纯解析，边界与文本路径同源（64 设备 / 248 字符名 / 地址校验后才可能变成
   命令参数）；`jwm-bridge` 不在时整条回退 `bluetoothctl` 原路径。
3. **顺带两处**：`BluetoothDevice.rssi` 落到 picker——同一 bond 状态内按信号
   排序、未配对行右侧显示原始 dBm（不折算百分比，一次广播的 RSSI 没有诚实的
   百分比换算；字形不涉及 U+F600 以上，`device_row` 守卫测试照常通过）。本机
   实测一次扫描返回 33 台设备、其中 31 台的「名字」就是自己的 MAC，按名字排
   等于按 MAC 排，信号是唯一有用的次序。另外 picker 的 `s`/`r` 现在与
   `ensure_connectivity_refresh` 一样合并在途任务，不再叠一串还在跑的扫描
   （真 `StartDiscovery` 是按连接引用计数的，叠加比浪费更糟）。

已知限制：本机有 hci0 / bluez 5.64 且已上电，`discover` 全链路真机实测通过
（含 RSSI）；但没有可配对的外设，所以 `Pair`→`Trusted`→`Connect` 这条路仍然
只有私有 dbus-daemon + 假 org.bluez 的集成测试覆盖（新增 `FakeDevice::Connect`
与可写 `Trusted` 属性，含「连不上仍算配对成功」一例）。

验证：fmt / clippy -D warnings / `cargo check --locked --all-targets` /
`--no-default-features` 及 7 组 backend feature profile 全绿；
`cargo test --locked` lib 2659 passed / 0 failed（基线 2654 + 新增 5：
connectivity 3、pairing 2）；
`cargo test --locked --manifest-path bridge/Cargo.toml` 49 passed / 0 failed
（基线 41 + 新增 8：discovery 纯函数 6、connect 预算 1、集成 1）。

---

## 2026-09-04：十一轮补充（蓝牙配对 v1 落地 / WM_HINTS 结案）

1. **蓝牙配对 v1 已实施**（上一轮的设计落地）。控制中心蓝牙 picker：`s` 发现
   扫描（有界 `bluetoothctl --timeout 15 scan on`，`[NEW]/[CHG]` 行解析），
   未配对设备 Enter 发起配对 → spawn 一次性 `jwm-bridge pair <addr>` 会话进程
   （zbus system bus，Agent1 @ KeyboardDisplay，只应答目标地址，入站授权一律
   Rejected，用完注销）。prompt UI 泛化为 `PromptKind::{Passphrase, Pin,
   Confirm, Display}`（masked/点名设备/y-n/仅展示），25s prompt 截止 + 95s
   会话硬墙钟；Esc/关面板/hand-over/返回 hub/点 scrim 全部汇入取消路径。
   IPC：commands `bluetooth_pairing_prompt`/`bluetooth_pairing_done`、query
   `get_bluetooth_pairing`、topic `bluetooth`（cookie 一次性防串台）。
   测试：jwm 侧 ~30 个纯逻辑单测；bridge 侧 41 个，含 `dbus-daemon --session`
   私有总线 + 假 org.bluez 的完整握手集成测试（confirm/拒绝实名错误/cancel/
   会话已死）。**未实测真机**（本机无蓝牙控制器），docs/control-center.md
   已换「out of scope」段。v2 候选：入站请求、配对后自动连接、D-Bus
   StartDiscovery。
2. **WM_HINTS 疑点结案**：见 v2c 节的复查记录——xprop 把属性类型写成
   INTEGER 导致 jwm 类型严格读取返回空，是测试工具假象；python-xlib 正确
   类型写入后 ICCCM urgency 置位/清除均正常。另注意：嵌套会话 debug! 日志
   不落盘，别再把「日志静默」当事件未到达的证据。

验证：fmt / clippy -D warnings / 两组 cargo check 全绿；root `cargo test
--locked` lib 2612 passed / 0 failed；bridge 41 passed / 0 failed。

---

## 2026-09-04：十一轮收口（HDR P0-4 帧尾 domain table + capture 独立 view）

**HDR P0 第 4 项完成**：帧尾 overlay 逐类 domain 归属集中到
`compositor/tail_domain.rs`（一张表驱动门禁/绘制位置/blocker 名）；snap preview、
expose、peek 迁移为 common-linear-aware（overview 本就已适配），其余 12 类保留具名
blocker 并接进 `api::LINEAR_TAIL_BLOCKER_NAMES`。capture/readback 改为从明确编码的
独立 view 派生（deferred 路由上 section 18c 派生专用 RGBA8 target，KMS 离屏 capture
换用该 view 纹理），`capture_readback` 不再出现在 route 决策里——capture 存在与否
scanout 逐位一致（headless oracle 钉住），PQ scanout 下 capture 仍得 canonical sRGB。
细节见下方 TODO 节「进展（2026-09-04 末）」。

验证：fmt / clippy -D warnings / 两组 cargo check 全绿；`cargo test --locked`
lib 2612 passed / 0 failed。

---

## 2026-09-04：十轮收口（HDR P0-3 internalize / urgent 标记 / 蓝牙配对设计）

1. **HDR P0 第 3 项完成**：cursor/DnD/layer-top/layer-overlay 四类外部元素现在
   internalize 进 FP16 common-linear target（lock 有意保持外部，锁定时整帧本就
   收敛 exact-sRGB）。staging 复用 Smithay 同一导入/采样路径合成 encoded-sRGB
   offscreen，compositor 用共享 window shader 的 legacy sRGB ingress 画入
   linear_fbo，帧尾逐输出 matrix+OETF 只施加一次；任意类失败 = 整帧回退
   exact-sRGB + KMS 照常组装（不丢元素不混域）。**顺带修了一个潜伏 bug**：
   scene-linear decode/encode 两个 fullscreen pass 的 V 采样方向相反导致
   deferred/early 路由上定位内容垂直翻转——此前 cursor 恒触发 fallback 使该
   路由从未真正驱动 scanout，所以从未暴露；已修并有窗口级回归。细节见下方
   TODO 节「进展（2026-09-04）」。像素 oracle（严格 surfaceless EGL，本机 Mesa
   真实执行）覆盖 SDR 与旧路径 ±1-2 LSB 一致、PQ 单次 OETF、partial damage
   移动无残影。
2. **网格 urgent 标记与跨屏评估**：见下方「2026-09-03：网格 v2c」节
   （该节日期实为 09-04 凌晨，保持原样）。其中 WM_HINTS 疑点已于 09-04
   复查结案：xprop 写错属性类型的测试假象，WM 链路无 bug（v2c 节有完整
   复查记录）。
3. **蓝牙配对设计完成**（只读调查，未实施）：推荐路线 = bridge 一次性配对会话
   进程（`jwm-bridge pair <addr>`，D-Bus Agent1，KeyboardDisplay capability，
   仅响应自己发起的配对），jwm 侧纯状态机 + prompt UI 泛化（wifi 密码 prompt
   先例）。v1 含发现扫描/PIN 输入/六位确认/仅展示；不做入站授权。完整计划
   在本会话记录中，实施时按「jwm 侧 pairing.rs + connectivity.rs +
   system_ui.rs prompt 泛化 + IPC 三个新命令 + bridge/src/bluez.rs」落地，
   CI 兜底 = 私有 dbus-daemon 挂假 org.bluez 跑完整握手。

验证：fmt / clippy -D warnings / 两组 cargo check 全绿；`cargo test --locked`
lib 2578 passed / 0 failed。

---

## 2026-09-03：网格 v2c — urgent 标记 + 跨屏评估

1. **urgent 标记**：`TagsGridCell.urgent`（api.rs:1256）+ `TagClientFrame.urgent`
   （tags_overview.rs:53）。聚合语义对齐状态栏 `calculate_tag_masks`：窗口在每个
   所属 tag 上计入（minimized/swallowed 同样计入），sticky / 全 tag 窗口浮在 tag
   轴之上、不标任何 cell；urgent ⊆ occupied 恒成立。绘制两端同位置同样式：
   `tags_grid::urgent_badge_rect`（标签带右端、随选中 cell 缩放）+
   `sysui_fill_rounded` 圆点，颜色用 `behavior.attention_color`（与 urgent 边框
   同 token，x11 render.rs:2206 / wayland_udev render.rs:4605 附近）。
2. **跨屏同框：评估后不做**。viewport 语义可以不动（单 panel 仍在 sel_mon），
   但其余全是结构改动：扁平 `TagsGrid` schema（单一 cols / 单一 live cell）、
   均匀 `grid_geometry`、矩形键盘行走、commit 只面向 sel_mon（跨屏需
   focusmon+view）、数字键跨组歧义、拖拽仅定义在窗口本显示器内。阻塞点与最小
   设计草案写进 docs/tags-overview.md 的 Limitations。

嵌套实测（v2c，/tmp/jwm-uivalidate/shots/v2c_*.png）：tag1 停放的 xclock 设
urgent → 开网格 cell 1 右上出现 attention 色圆点（裁片 AE diff 60），清除后
消失（diff 0）。**WM_HINTS 疑点已结案（09-04 复查）**：当时的「xprop 直改
不触发」是测试工具假象——`xprop -f WM_HINTS 32i -set` 会把属性类型写成
INTEGER（回显 `WM_HINTS(INTEGER)` 可辨），jwm 的 `get_property` 按
WM_HINTS 类型严格读取，类型不符返回空、静默 no-op，这是正确行为。
python-xlib 以正确类型（XA_WM_HINTS=35）写入后，urgency 置位/清除即时生效
（含聚焦自动抑制 + 清除源位的策略分支）。EWMH 与 ICCCM 两条路径都正常，
无代码改动。附带教训：嵌套会话里 debug! 级日志不落盘（RUST_LOG=debug 也
只有 info 行），「日志静默」不能作为事件未到达的证据。

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

**网格总览剩余**：urgent 标记已由 v2c 完成（见上方 2026-09-03 v2c 节）；
跨显示器同框评估为结构性改动、不做，阻塞点与最小设计草案见
docs/tags-overview.md 的 Limitations。WM_HINTS 疑点已结案（xprop 类型假象，
v2c 节有复查记录）。

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

### 进展（2026-09-04 末三）：P1-5 tone-map 定义层已接进渲染决策点

末二条目记录的「第 5 条剩余」三件已同批落地（ingress 与 delivery 同批，
不存在 PQ 内容 49 倍亮度的中间态）。

**(a) ingress rescale。** `ColorTransform::build_to_linear_srgb` 把
`working_space_scale(tf)` 折进 gamut matrix（标量逐通道一致，被 matrix
吸收，shader 无第二次乘法）：PQ 内容解码后按 10000/203、HLG 按
1000/203 落进工作空间。SDR 系 scale=1.0，`x * 1.0 == x` 在 IEEE-754
下逐位相等，既有 SDR ingress 矩阵逐位不变（新单测
`linear_srgb_ingress_rescale_is_bitwise_identity_for_the_sdr_family`
对 sRGB/Gamma22/BT.1886/Power × sRGB/BT.2020 组合逐位钉住，HDR 系钉
scale·gamut）。legacy encoded fallback 仍走 `ColorTransform::build`
不受影响。逐窗口 shader（window/cube 共用 `u_color_matrix` 契约）自动
继承，无 GLSL 改动。

**(b) delivery tone-map。** 新类型 `OutputToneMapPlan { policy,
source_peak_working, target_peak_working }`（color_pipeline.rs），
`for_output(source_peak, output_tf)` 是唯一选型点（target =
`working_space_scale(output_tf)`，同时充当 rescale 除数——把 working
值重锚定到输出 TF 自身的归一化标尺后过 OETF），`IDENTITY` = 直通+单位
除数。`OutputColorRegion` 携带 plan；帧尾 encode shader
（`SCENE_LINEAR_ENCODE_FRAGMENT`）新增 `u_tone_map`（0=ReferenceWhite
直通=GL 零初始化默认值）、`u_source_peak`、`u_target_peak`（≤0 视为 1.0，
与 encode_tf=-1 的既有默认约定同构），在 gamut matrix 之后、输出 OETF
之前按通道施加——tone-map 非线性不能折进 matrix。GLSL 与 Rust 的
`map_working_linear`/`map_to_output_scale` 数学逐项对应（含 Reinhard
退化回 clip）。`dispatch_output_color_regions` 在 `hw_encode_active`
时替换 IDENTITY plan（硬件 LUT 已含 rescale，shader 不得二次施加）；
transition snapshot、early-sRGB fallback、overview re-entry、capture
view 四个既有 encode 调用点全部传 IDENTITY，字节行为不变。

**per-frame 峰值数据通道（新建）。** backend.rs 帧循环在 delivery
decision 之前新增纯函数 `aggregate_output_source_peaks`：full_scene 窗口
矩形 × 输出逻辑矩形交集，取各输出可见窗口 committed image description
的 `working_space_scale(tf)` 最大值，下限 1.0（全 SDR 输出恒直通 =
改动前行为；未描述 surface 与 staged 外部元素按 SDR 计 1.0——staging 本就
拒绝非 sRGB 描述，描述变更经 generation 钟触发重绘）。仅在
cm_render_gate && scene_linear_color_path 时计算（snapshot 复用同一帧
surface plans 的那份，仍一帧一次锁），经
`refresh_color_pipeline_offload` 新参数 `source_peaks: &HashMap<String,
f32>` 进 KMS，`OutputColorRegionCandidate` 携带进
`plan_software_color_regions` 落成各 region 的 plan；`same_profile`
重叠检查含 tone_map（镜像输出峰值冲突整体拒绝，fail-closed）。
`OutputColorFrameState` 比较自动含 plan——峰值变化（HDR 窗口出现/跨
输出移动/描述变更）即使几何不变也触发 full damage 重编码。

**硬件路径 LUT 烘焙。** `build_gamma_lut_delivery(tf, plan, size)` 通用
烘焙（tone-map + 重锚定 + OETF）；`build_gamma_lut_scanout(tf, size)`
是 scanout  canonical 曲线，`create_gamma_lut_blob` 换用它。关键结构
性事实（单测 `delivery_lut_clip_matches_scanout_curve_over_the_fb_domain`
钉住）：在 FB 归一化域 [0,1] 上，`for_peaks` 可达的全部策略
（ReferenceWhite 直通、Clip 在 target≥1 恒不触发）与 rescale OETF 逐
字节一致，域外值由硬件 LUT 索引钳制——正是 Clip。因此
`installed_gamma_lut` 仍以 `TransferKind` 为键，无逐帧 blob churn；
SDR 系 LUT 与旧 `build_gamma_lut` 逐字节一致（
`scanout_lut_is_byte_identical_to_legacy_ramp_for_the_sdr_family`），
PQ/HLG 输出 LUT 末项锚定 203 nits（
`scanout_lut_reanchors_hdr_transfers_onto_reference_white`）。将来若把
ReinhardShoulder（峰值相关）接上硬件，必须先扩展该键——已注释在
`build_gamma_lut_scanout` 文档。

**oracle 修正说明（仅 HDR 路由）。** headless_render 两处 PQ region 断言
（internalized 元素 Frame C、capture-view Frame C）的 CPU oracle 现在
含 delivery 级（tone-map + rescale），与 region 安装的 plan 共享同一
`OutputToneMapPlan`——SDR 内容在 PQ 输出上重锚定到 203 nits（改动前
oracle 把 working 1.0 当 PQ 满量程 10000 nits，正是本轮要消除的错
锚）。断言本身（tolerance、语义注释意图「单次 OETF」）未动；全部 SDR
路由 oracle 原样通过，且 identity plan 经新 shader 代码路径与原样零
初始化 uniform 渲染逐字节相等（新 GL 测试
`wayland_scene_linear_encode_tone_map_matches_cpu_oracle` 同时钉
PQ 锚点 148/255、Clip 上限、Reinhard 高光层次）。

HDR enable 语义、direct scanout 阻断、末二的 KMS 原子交付均不变；HDR
路由仍在 fail-closed 门后（`params_for_output` 在 HDR 未启用时恒 sRGB，
故所有可达输出 TF 为 SDR，上述 PQ/HLG 交付路径为正确性预留）。验证：
`cargo fmt --all -- --check`、`cargo test --locked`（lib 2654 passed =
基线 2641 + 新增 13：color_pipeline 9、udev_kms 2、backend 聚合 1、
GL shader 1；其余 target 全绿）、`cargo clippy --locked --lib --bins
--tests --no-deps -- -D warnings`、`cargo check --locked --all-targets`
与 `--no-default-features`（0 警告，既有的 5 个 X11 clipboard dead-code
警告已不复存在）、`cargo test --locked --manifest-path
bridge/Cargo.toml`（41 passed）全绿。无显示会话，行为验证全部来自
colocated 单测与严格 surfaceless EGL oracle。

### 进展（2026-09-04 末二）：P1-6 KMS 原子交付与位深门槛已完成

**受控原子请求装配层（纯逻辑，可单测）。** `udev_kms.rs` 新增：
`AtomicColorAssignment`（raw object/property/value，装配与测试不需要 live
device）、`ScanoutColorPropertyHandles`（init 时一次性探测并缓存的 CRTC
DEGAMMA_LUT/CTM/GAMMA_LUT 与 connector Colorspace——含 BT2020_RGB 枚举值——及
HDR_OUTPUT_METADATA 句柄）、`ScanoutColorTarget`（`None`=不进请求、
`Some(0)`=清中性、`Some(v)`=安装）、`build_atomic_color_request`（逐输出固定
顺序 DEGAMMA→CTM→GAMMA→Colorspace→HDR_OUTPUT_METADATA；安装到不存在的属性=
硬错误，清除不存在的属性=no-op）、`commit_atomic_color_request`（单个
`AtomicModeReq`，TEST_ONLY 后正式 commit；legacy-only 设备直接拒绝而不是退回
逐属性 ioctl——与 init 时中性 reset 同一条 fail-closed 规则。注意：此前
legacy 驱动尚可经 `set_drm_property` fallback 安装 LUT/CTM，现在保持软件
SDR，属有意的 fail-closed 行为差异）。

**delivery group 单次提交。** `refresh_color_pipeline_offload` 的 CRTC stage 激活
从「逐输出逐属性 commit + 软件 rollback 循环」重写为
`apply_scanout_color_goals`：先为所有状态有变化的输出建齐新 blob（blob 在被
commit 引用前是惰性的），再用一次 TEST_ONLY+commit 覆盖全部输出——内核原子性
取代旧的 rollback 模拟，任何失败都不会有半切换状态落到 scanout，新 blob 全部
销毁、tracked state 不动。失败后再补一次全清零请求让软件路由在已知 domain 下
继续；清零也失败则由 `finish_color_pipeline_decision` 的一致性检查阻断呈现直到
重试成功（与原 fail-closed 语义一致）。DEGAMMA 在同一请求里钉中性；
`refresh_output_color_targets` 的 stale CTM+LUT teardown 同样改为一次原子清零。
decision 的 hw 标志改由提交后的实际 tracked state 推导（报告硬件现在拥有什么，
而不是请求了什么）。`install_gamma_lut`/`install_ctm`/`ctm_offload_allowed`
随之删除——CTM 必须配 OETF 的约束已由单次提交结构化保证。

**FB 配对（结构性说明）。** Smithay 的 `DrmCompositor` 内部拥有 FB commit，没有
注入额外属性的钩子（已核对 vendored 源码 `drm/compositor`、`drm/surface`），FB
无法并入同一个 ioctl。配对保证因此由「顺序 + 证据」构成：颜色请求严格先于该帧
FB queue 提交，`invalidate_color_delivery_after_hardware_change` 使 last-success
在颜色变化后立即失效，只有变化之后入队的帧到达 vblank 才会报告硬件路由；同时
swapchain format 已进入 HDR 链验证（见下），被扫描 FB 的位深仍在受控判定之内。
P2 若要字面意义的「FB+属性同一请求」，需要绕开 `DrmCompositor::queue_frame`
自行提交 plane state——本轮未做，在此明确记录。

**10-bit 链验证（fail-closed）。** 纯函数 `hdr_scanout_chain_gap` 按稳定优先级
返回 `HdrScanoutChainGap`（CrossDevice → FramebufferBitDepth →
PlaneFormatUnsupported → CrtcColorStagesMissing → ConnectorColorspaceMissing →
ConnectorHdrMetadataMissing）：要求同一 DRM device、swapchain FB ≥10-bit
（2101010/16161616f 系列，未知格式拒绝）、primary plane 支持该精确格式、CRTC
具备 GAMMA_LUT+CTM、connector 具备 Colorspace 且枚举表含 BT2020_RGB、具备
HDR_OUTPUT_METADATA。init 按输出缓存 `swapchain_fourcc`
（`DrmCompositor::format()`）与 `primary_plane_formats`
（`plane_info().formats`）。`set_hdr_metadata_for_output` 的 enable 分支先跑链
验证，任何缺口直接拒绝、保持软件 SDR、不标记 active（backend.rs 的
compositor 级 fail-closed 门禁不变，KMS 层自身同样 fail-closed）；signalling
本体改为 Colorspace+HDR_OUTPUT_METADATA 同一受控请求（enable 时
Colorspace=BT2020_RGB，disable 时双双回 Default/0），失败销毁新建 blob。一个
小的行为差异：disable 路径在 connector 没有任何颜色属性时现在返回 Ok
（无操作）而不是 "property not found"——该路径此前只在从未 enable 成功过
的硬件上被用户显式 toggle-off 时触发。direct scanout 阻断与 HDR enable 的
fail-closed 语义不变；SDR 输出行为与路由决策不变。

单测（新增 10、删除 1 个随 `ctm_offload_allowed` 移除的）：请求装配的顺序/
多输出单请求/安装到缺失属性报错/清除缺失属性 no-op/未触碰 stage 不进请求；
位深分类（8/10/16/未知格式）；链验证全通、逐缺口 fail-closed 与优先级；
goal↔tracked state 匹配矩阵。验证：`cargo fmt --all -- --check`、
`cargo test --locked`（lib 2641 passed = 基线 2632 + 净增 9）、
`cargo clippy --locked --lib --bins --tests --no-deps -- -D warnings`、
`cargo check --locked --all-targets`（默认与 --no-default-features，后者仍只有
5 个既有 X11 clipboard dead-code 警告）、`cargo test --locked
--manifest-path bridge/Cargo.toml`（41 passed）全绿。无显示会话，行为验证
全部来自 colocated 单测。

**第 5 条剩余（tone-map 接线）本轮未动，精确记录待办：** 定义层接进渲染决策点
需要 (a) ingress：`build_to_linear_srgb` 把 `working_space_scale(tf)` 折进 gamut
matrix（标量可被 matrix 吸收，SDR 系 scale=1.0 时逐位不变）；(b) delivery：逐
输出 plan 需要该输出可见 surface 的 source peak 聚合——这条 per-frame 数据通道
目前不存在（`snapshot_surface_params` 只供 IPC），然后
`ToneMapPolicy::for_peaks(source_peak, working_space_scale(output_tf))` 选型、在
输出 OETF 前施加 `map_working_linear`——软件路径要改 scene-linear encode
shader（新增 rescale/policy uniform；tone-map 是逐通道非线性，不能折进
matrix），硬件路径要把 rescale+tone-map 烘进 `build_gamma_lut` 的 LUT 曲线；
(c) 验收硬约束：SDR→SDR 与 SDR→HDR-with-SDR-output 每条既有路由逐像素
identity（headless_render 现有 oracle 原样通过），HDR 路由仍在 fail-closed
门后。ingress 与 delivery 必须同批落地，单独改 ingress 会让 PQ 内容在
delivery 未接 rescale 时亮度错 49 倍。

### 进展（2026-09-04 后半）：P1 前半——image-description commit latch 与亮度/tone-map 定义层已完成

**Commit latch（纯协议状态机）**。surface 的 image description 从「协议请求即生效」
改为标准双缓冲：新纯状态机 `SurfaceDescriptionLatch`（pending/current 两半，
`Some(None)` 为 staged 移除）内嵌于 `ColorManagerState` 的逐 surface 表
（`color_management.rs`）。`set_image_description` / `unset_image_description` 只
stage 进 pending；`CompositorHandler::commit` 调用 `commit_surface_description`
锁存进 current——smithay 在事务 apply 时逐 surface 触发该 hook（普通 surface 与
desync subsurface 立即、sync subsurface 随 parent commit 触发），正好是协议对整棵
surface 树的 commit 语义，subsurface 同步/异步无需额外分支。渲染/计划/IPC 仍只读
current（`snapshot_surface_params` / `snapshot_surface_descriptions`），generation
失效钟只在 committed 半区真正变化时推进（同 identity 重提交幂等）。`destroy` 按
协议等于 unset（双缓冲 staged 移除，不再立即生效）；wl_surface 本体销毁时
`CompositorHandler::destroyed` 清掉 latch 两半与 feedback bucket（顺带修了
wl_surface 先死而 cm 对象不毁时描述与 feedback 滞留的泄漏）；cm 对象 `destroyed`
钩子只在 wl_surface 已死时兜底。colocated 单测覆盖：set→commit 生效顺序、set 两次
commit 一次（后者胜出）、commit 无 set 保持、unset 双缓冲、set+unset 提交前抵消、
同 identity 幂等、destroy 清理 pending+current、idle 条目回收、generation 只在
committed 变化时推进。

**亮度基准与 tone-map 定义层（纯数学，未接入渲染，输出像素不变）**。
`color_management::SDR_REFERENCE_WHITE_NITS = 203.0`：工作空间线性 1.0 的绝对锚点，
取 BT.2408 HDR reference white（而非 scRGB 的 80 nits 惯例），与
`color_policy::params_from_edid` 已外发的 `reference_lum = 203` 一致，选型依据写在
常量文档。`color_pipeline` 新增：`PQ_MAX_LUMINANCE_NITS`(10 000)、
`HLG_NOMINAL_PEAK_NITS`(1 000)、`pq_encode_nits`/`pq_decode_nits`、
`hlg_encode_nits`/`hlg_decode_nits`、`nits_to_working_linear`/
`working_linear_to_nits`、`working_space_scale(tf)`（PQ→10000/203、HLG→1000/203、
SDR 系→1.0），以及 `ToneMapPolicy`（`ReferenceWhite`/`Clip`/`ReinhardShoulder`）+
`for_peaks` 默认策略（装得下目标峰值→ReferenceWhite，否则 Clip）+
`map_working_linear` 纯映射（Reinhard 为锚定 source peak 的 extended 形式，退化
峰值回退 clip）。决策点已写进模块文档但**未接线**：ingress 端
（`build_to_linear_srgb` 乘 scale 因子）与逐输出 delivery plan 端（`for_peaks`
选型、`map_working_linear` 施加在输出 OETF 之前）。HDR enable 仍 fail-closed。
单测覆盖 PQ/HLG nits 往返与已知参考点（100 nits↔0.5081、203 nits↔0.5807、HLG
0.75↔265 nits）、working↔nits 锚点、逐 TF scale、policy 选型矩阵、clip 边界、
Reinhard 端点/单调性/高光层次保持/退化峰值回退。

验证：`cargo fmt --all -- --check`、`cargo test --locked`（lib 2632 passed =
基线 2612 + 新增 20）、`cargo clippy --locked --lib --bins --tests --no-deps --
-D warnings`、`cargo check --locked --all-targets`（默认与 --no-default-features）
全绿；无显示会话，行为验证全部来自 colocated 单测。

### 进展（2026-09-04 末）：P0-4 帧尾 domain table + capture 独立 view 已完成

帧尾 overlay 的颜色域归属集中到新模块 `compositor/tail_domain.rs`
（`TailOverlayClass` 16 类，逐类记录 domain / 具名 blocker / 绘制位置 stage），同一张
表驱动门禁（`linear_tail_status`）、绘制域（post-delivery 绑定走
`bind_post_delivery_overlay_target`）与 KMS 侧 blocker 清单
（`linear_tail_blocker_names`），不再有第二套枚举。迁移进 common-linear-aware 的四类：
snap preview、overview（本就已适配）、expose、peek——它们在 deferred 路由上直接画进
linear_fbo（border/overview_bg/window 三个共享 shader 的 `u_scene_linear` 入域），统一走
帧尾单次 matrix+OETF。保留具名 blocker 的 12 类（全部接进
`api::LINEAR_TAIL_BLOCKER_NAMES`）：workspace_transition_overlay、tab_bar_overlay、
particle_overlay、edge_glow_overlay、postprocess_filter、debug_hud_overlay、
annotation_overlay、screenshot_toolbar_overlay、toast_overlay、osd_overlay、
system_ui_overlay、recording_region_overlay。`compositor_encoded_tail` 现在只在
common-linear target 本身不可用时发出。

capture/readback 与 route 解耦：截图/录屏/screencopy 一律读「独立编码 view」——
legacy/early-fallback 路由上 output_fbo 本身就是 canonical sRGB view；deferred 路由上由
render.rs section 18c 用一次 identity-matrix sRGB OETF 从 linear_fbo 派生专用 RGBA8
capture target（懒分配）。KMS 离屏 capture 渲染元素列表时把 compositor 元素临时换到该
view（随后复原），compositor 侧截图/录屏 readback 也改读该 view；`capture_readback`
blocker 从 route 决策中删除（名称保留在已知表中兼容历史 payload）。像素 oracle 证明：
capture 请求存在与否 scanout 像素逐位一致；PQ scanout 下 capture view 仍是 canonical
sRGB。顺带修掉一个本改动会引入的回归：cursor internalized 的帧里录屏不再叠加合成箭头
（`capture_frame` 新增 `cursor_already_present`）。

### 进展（2026-09-04）：P0 internalize/adapt 已完成

cursor（主题位图与软件 fallback）、DnD drag icon、layer-top、layer-overlay 四类外部
元素现在 internalize 进 FP16 common-linear target；session-lock 保持 KMS 外部组装
（遮挡安全语义：锁定帧稀少、exact-sRGB 无视觉损失，shield/lock 路径维持单一受审计
边界；锁定时帧尾本就因 lock blocker 收敛 fallback，游标也随之留在外部，不混域）。

- **z-order**：staging 按 KMS 元素 vec 的 front-to-back 顺序（cursor、dnd、overlay、
  top）收集，reverse 后交给 compositor 以 back-to-front 画入 linear_fbo——与 KMS
  组装相对 z 完全一致（cursor 最顶）。internalized 元素的 enter/leave 与 frame
  callback 簿记改由 `shows()` 门控保留在 `render_if_needed`，仅元素推送由
  `assembles()` 门控。
- **适配器复用**：staging 用 Smithay 既有 import + `render_output` 把每棵树/位图
  合成到 offscreen premultiplied encoded-sRGB texture；compositor 以共享 window
  shader 的 legacy sRGB ingress（`color_managed=0 + scene_linear=1`）绘入
  linear_fbo，无第二套传递函数；既有逐输出 matrix + OETF 在帧尾统一施加一次。
  无颜色描述按 sRGB ingress；带非 sRGB 默认描述的树（PQ/HLG/广色域）staging
  拒绝并留在 KMS 路径（`description_is_srgb_default`）。
- **每帧 fail-closed**：`commit_staged_internalization` 只在 staging 成功且 trial
  plan 安全时才把 disposition 从 `ExternalAssembly` 迁到 `Internalized`；任一失败
  整帧退回 exact-sRGB 且该类回到 KMS 组装（`ImportBlocked` 永不迁移）。verdict 经
  `KmsState.internalized_external_frame` 以 (texture id, generation) 钉住当前
  compositor 纹理，assembly 与 route 决策共用同一计划语义。
- **顺带修复的潜伏缺陷**：scene-linear decode/encode pass 的 V 采样方向曾使
  linear_fbo 与 output_fbo 行存储约定相反，所有定位内容（含窗口）在
  deferred/Early 路由上垂直翻转——该路由此前从未真正驱动 scanout（cursor 恒阻塞
  触发 fallback），故未暴露；两 shader 现统一 V 翻转使两 FBO 同一约定。
  `wayland_scene_linear_route_preserves_window_scene_position` 钉住契约。
- **damage/scanout**：internalized 元素的 prev∪curr 矩形注入 dirty tracker
  （section 1c），partial-damage 修复下移动无残影；`external_elements_drawn`
  期间 direct scanout 保持阻断。
- **像素 oracle**（严格 surfaceless EGL）：SDR 全帧 opaque/背景与旧路径 ±1 LSB
  一致；半透明列按域分别钉住 CPU oracle（internalized=线性混合、legacy=编码
  混合，线性混合是管线既定语义）；PQ region 验证单次 OETF；重叠区验证
  back-to-front；partial-damage 下跨位置移动验证无残影。策略侧覆盖 disposition
  迁移矩阵、staging 候选/门控、commit 全有或全无、verdict identity 钉住。
- **P0-4 接口预留**：`produces_pixels`/`assembles_externally`/`contributes_blocker`
  的三分语义、`assembly` 的 `common_linear` 取值与 verdict 通道即帧尾 domain
  table 的接入点；encoded-tail overlay 仍各自保留具名 blocker。

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

**[2026-09-04 更新] 队列已走完 P0–P2。** enable 现在是条件式的：逐输出
`hdr_enable_refusal` 门禁 + `hdr_requested` 意图锁存 + 每帧 assert/withdraw
和解，首个切片限定在 software per-output region 路由。剩余非目标（硬件路由的
FP16 scanout FB 与扩展 LUT 键、FB 与颜色属性的单 ioctl、`linear_tail_safe`
逐输出化）见「十二轮收口之二」条目与 docs/hdr.md。

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
4. **[已完成] 帧尾仍有第二套颜色域。** Expose/Peek、tabs、particles、edge glow、HUD、annotation、
   toolbar、toast/OSD、recording overlay 等必须逐类标注为 common-linear-aware，或保留具名
   blocker；capture/readback 要从明确编码的独立 view 派生，不能通过改变物理 scanout route
   来获得截图。
5. **真实 HDR 语义尚不完整（定义层接线已完成）。** working-white/absolute-luminance 基准
   （SDR white = 203 nits）、SDR/PQ/HLG 标尺互转与 tone-map policy 已落地，
   且已接进渲染决策点：ingress rescale 折进 `build_to_linear_srgb` 的
   gamut matrix（SDR 系逐位不变），delivery 端逐输出 plan
   （`OutputToneMapPlan` + 新建 per-frame 峰值聚合通道）在输出 OETF 前
   施加——软件 encode shader 新 uniform、硬件 GAMMA_LUT 烘焙均已接线，
   接线细节与 SDR 逐像素 identity 验收见 2026-09-04 末三的进展条目；
   image-description 已与对应 `wl_surface.commit` 原子锁存
   （pending/current 双缓冲，subsurface 同步语义由 smithay 事务 apply 点
   继承）。非 D65 白点已 Bradford 适应；HDR enable 继续 fail-closed。
6. **[已完成] KMS 交付的原子事务与位深门槛。** CRTC color stages
   （DEGAMMA/CTM/GAMMA）现在对整个 delivery group 以单个 TEST_ONLY + atomic
   commit 编程（`apply_scanout_color_goals`，内核原子性取代软件 rollback），
   connector `Colorspace`/`HDR_OUTPUT_METADATA` 经同一机制单次提交
   （`set_hdr_metadata_for_output`）；HDR enable 之前先过 `hdr_scanout_chain_gap`
   的 10-bit（或更高）format/plane/connector 链验证，任何缺口或跨 DRM device
   都保持软件 SDR 且不宣称 hardware HDR active。FB 本身由 Smithay
   DrmCompositor 内部提交（无属性注入钩子），FB 与颜色属性的配对由「颜色请求
   严格先行 + last-success 失效钟」保证，swapchain format 位深由链验证覆盖。
   direct scanout 在未证明 profile passthrough 正确前继续阻断。

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
3. **P0：[已完成] internalize/adapt** — `src/backend/wayland_udev/backend.rs` 与
   `compositor/{render,mod}.rs`
   - 优先把上述元素按正确 z-order 绘入 FP16 common-linear target，再执行现有逐输出
     matrix + OETF；保留 exact-sRGB fallback 作为每帧 fail-closed 路径。
   - 复用 `color_management.rs` / `color_pipeline.rs` 的 sRGB ingress、矩阵布局和 transfer
     规则，不为 cursor/layer 复制另一套 GLSL 传递函数。
4. **P0：[已完成] 清理其余 linear-tail blocker** — `compositor/{tail_domain,damage,render,expose}.rs`
   - 建立帧尾 domain table，让每一类 overlay 要么在 final delivery 前绘制，要么有显式颜色
     adapter；capture/recording 使用独立、目标明确的 view，不再反向约束物理输出 route。
5. **P1：[已完成] 补齐颜色语义** — `color_management.rs`、`color_pipeline.rs` 与 surface commit 路径
   - 非 D65 Bradford CAT 已实现并测试。已完成：working white/absolute
     luminance 基准（SDR white = 203 nits）、SDR/PQ/HLG 标尺互转与 tone-map
     policy；image-description pending/current 双缓冲只在匹配
     surface commit 锁存。定义层已接进渲染决策点：ingress scale 折进
     `build_to_linear_srgb`，delivery 端逐输出 tone-map plan 经新建
     per-frame 峰值通道驱动软件 encode shader 与硬件 LUT 烘焙
     （见 2026-09-04 末三进展条目）；SDR 路由逐像素 identity 由既有
     oracle 原样通过与新增 identity/LUT 一致性测试双重钉住。
6. **P1：[已完成] KMS 原子交付与位深** — `src/backend/udev_kms.rs`
   - CRTC color stages 与 connector signalling 各自由同一受控 atomic
     request（TEST_ONLY + commit）编程；跨 DRM device 或 10-bit 链路不完整时
     继续软件 SDR，不宣称 hardware HDR active。FB 与颜色属性的同一请求受
     Smithay DrmCompositor 结构限制，配对语义见进展条目。
7. **P2：[已完成] 开放真实 HDR enable** — `src/backend/udev_kms.rs` 与
   `src/jwm/ipc_handler.rs`
   - `hdr_enable_refusal`（纯函数，11 个具名 refusal，硬件→配置→本帧内容的
     固定优先级）是唯一判据，IPC 请求与逐帧和解共用它；`hdr_requested` 锁存
     意图，`hdr_signalling_action` 每帧比对「意图 × 拒绝 × 现状」→
     Hold/Assert/Withdraw。DPMS、gamma-control、tail 不安全、路由变化、
     participation 变化全部自动撤回并在恢复后自动重新断言。
   - 首个切片**只允许 software per-output region 路由**：硬件 CRTC pair 路由
     把 working-linear 写进 RGB10_A2/RGBA8 unorm FBO，参考白以上全部在
     GAMMA_LUT 之前被裁掉——那恰好就是 HDR 存在的意义；且
     `installed_gamma_lut` 以 `TransferKind` 为键，烘不进峰值相关策略。
     解除需要 FP16 scanout FB + 扩展 LUT 键，见 docs/hdr.md 与下方非目标。
   - FB 与颜色属性仍非同一 ioctl（Smithay 结构限制），配对语义仍是
     「颜色请求严格先行 + last-success 失效钟」，这一条在 docs/hdr.md 的
     Limitations 里写明，不再冒充完成。
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
