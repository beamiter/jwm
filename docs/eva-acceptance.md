# Eva 实施与验收账本

本账本对应 `JWM_Eva_Voice_Assistant_Design_v1.md`。源码、编译通过、本地确定性测试、真实桌面验证和真实模型验证是不同证据；只有覆盖相应要求的证据才可标为通过。本文件不把未接入 API 的测试写成云端成功，也不把全部名字已登记写成全部用户能力已验收。

## 验收状态与证据边界

- `已实现`：对应执行路径存在，不等于设备或模型实测通过。
- `本地通过`：下列实际执行记录覆盖相应边界；mock 后端不证明真实设备完成。
- `待实测`：需要登录桌面、真实设备或用户提供的 API 凭据。
- P4 的唤醒词、AEC、自然打断和 jagent 是方案明确留待后续的扩展；首版必须保持本地唤醒词关闭。

以下冻结矩阵记录 2026-10-29 的初始本地验收。2026-10-30 的用户 X11 会话、真实麦克风与 API 基础联调另见文末；未覆盖的模型语料、设备和性能指标继续保留，不能以合成图、mock 或 IPC 测试替代。

## 本地可重复检查

| 命令/检查 | 检查范围 | 当前证据 |
|---|---|---|
| `cargo check -p jwm --all-targets --offline` | 默认 X11RB/XCB/Wayland 的库、程序和测试目标，包括 Eva 协议与浮层 | 冻结源码通过，无警告；日志 `jwm-eva-frozen-default_all_targets.log` |
| `cargo clippy -p jwm --all-targets --offline -- -D warnings`、`cargo build -p jwm --bin jwm --offline` | 最终 JWM 源码与可运行二进制 | 全部通过；冻结依赖源码 SHA 首尾一致 |
| `cargo check -p jwm --lib --no-default-features --offline` | 精简构建不依赖音频/云端运行时 | 已由以下精简服务端测试实际编译和运行覆盖 |
| `cargo test -p jwm_eva_core --offline` | 计划、代次、VAD 跟进截止时间、账本、费用、登记 | 最终整合实跑 35 passed / 0 failed；不以测试数量推定真实语义成功率 |
| `cargo test -p jwm --lib jwm::eva::tests --offline -- --test-threads=1` | 默认构建上的定向执行、取消、实例重用、确认、媒体读回、剪贴板所有权和撤销补偿 | 冻结源码 32 passed / 0 failed / 0 ignored，4.61 s |
| 同上并加 `--no-default-features` | 精简构建上的同一真实 JWM 数据模型与 mock 后端故障边界 | 冻结源码 32 passed / 0 failed / 0 ignored，4.65 s；包含跨进程 Unix socket 取消 |
| `JWM_REQUIRE_HEADLESS_GL=1 EGL_PLATFORM=surfaceless cargo test -p jwm --lib eva_ -- --test-threads=1` | 输入路由、字体/视口审阅签名、浮层状态、主题动画、实际宿主可见性与无显示服务器 GPU 浮层 | 冻结源码 15 passed / 0 failed / 0 ignored，3.97 s；GLES 与私有 Xvfb 均实际执行 |
| `cargo test -p jwm --lib native_owner_metadata_multiple_and_direct_round_trip --offline -- --test-threads=1` | 私有 Xvfb 的 X11RB/XCB 原生 clipboard offer、实际取得所有权和内容读取 | 冻结源码 2 passed / 0 failed / 0 ignored，0.28 s |
| `cargo test -p jwm_ipc_client --offline` | 独立连接、逐行有界 frame、超时和订阅路径 | 17 passed / 0 failed / 0 ignored |
| `cargo +1.89.0 test -p jwm-eva --lib --offline -- --test-threads=1` | daemon、音频、配置、费用只读入口、PCM 输入归属与 loopback WebSocket 真 pipeline | 最终整合 79 passed / 0 failed / 2 ignored；两个忽略项在下面私有 native 图中另行实际通过 |
| `cargo +1.89.0 clippy -p jwm-eva -p jwm_eva_core -p jwm_ipc_client --all-targets --offline -- -D warnings` | 全部 daemon/core/client 编译目标 | 最终检查通过，无警告 |
| `cargo +1.89.0 check --locked -p jwm-eva --all-targets --offline` | 最低 Rust 版本，使用独立 target 和兼容 PipeWire 前缀 | 检查通过；最终源码还实际执行上述 Rust 1.89 全套测试与 Clippy |
| `cargo test -p jwm-eva --test daemon_ipc --test executor_ipc --test undo_ipc --test packaging --offline` | 真 daemon/0600 socket/单实例/SIGTERM、配置不一致拒绝与坏配置仍可取消；真串行 executor 的部分失败/丢回执/21 条旧未知回执公平结算；逆序补偿与新修改屏障；安装配置加载 | 分别 1/2/2/1 项通过；JWM 端为故障注入 stand-in，不能视为真实桌面或设备成功 |
| `bash scripts/test-install-eva.sh`、`shellcheck`、已渲染 service 的 `systemd-analyze --user verify` | 重复安装/升级保留配置、空格与 `%` 路径、隔离 systemctl 失败路径、服务语法 | 通过；不替代用户图形会话实际登录/登出 |
| `git diff --check` | 补丁空白错误 | 本报告生成前无错误；不是功能验收 |
| `python3 eva/tests/desktop-ipc.py --jwm target/debug/jwm --eva target/eva-msrv/debug/jwm-eva --keep --report /tmp/jwm-eva-desktop-final-report.json` | 私有 Xvfb/Xephyr/DBus/runtime 中运行真 WM/Eva；Idle 资源、原生剪贴板和真实 IPC | X11RB、XCB、Wayland-winit 各 7/7 通过；Wayland-X11 缺 DRI3 启动失败、0 步，未计为通过；整个四后端命令退出 1，原因照实保留 |
| `eva/tests/native-audio.sh`（客户端/服务端私有 PipeWire 1.2.8） | 合成正弦源+静音 sink；可信采集时间戳、停止、真实 drain、中断历史及本地提示音完成 | 最终 Eva test artifact 上 2 passed / 0 failed / 0 ignored；采集释放 12 ms、200 ms 音频 drain 235 ms；运行约 350 ms 后停止，时钟冻结读回 330 ms，中断 6 ms，partial item 截断 141 ms，heard 保留且 unheard 删除；无硬件/API |
| 同用例，私有 graph/source 改为 48 kHz，客户端仍为 24 kHz | 真实 PipeWire 重采样、源时间戳与播放时钟换算 | 最终 test artifact 上 2 passed / 0 failed / 0 ignored；采集释放 12 ms、drain 225 ms、冻结播放位置 336 ms、中断 8 ms、partial 截断 141 ms；复现副本在 `artifacts/eva/native-audio-48k-repro.sh` |
| `JWM_REQUIRE_DBUS_DAEMON=1 cargo test -p jwm-bridge --bin jwm-bridge mpris::tests --offline -- --test-threads=1` | 真私有 DBus；所有媒体命令冻结 unique owner/PID；资源、track、seek 和 native open 的关联回执，活动播放器变化不改目标 | 35 passed / 0 failed / 0 ignored |
| `RUN_JWM_MPV_TESTS=1 cargo test -p jwm-bridge --bin jwm-player --offline -- --test-threads=1` | 真 headless mpv、私有 DBus、并发单实例、同 URI 不同队列 entry、重送零新增、错误 owner 零调用、丢回执不重放与实际 Playing | 16 passed / 0 failed / 0 ignored；不使用真实声卡或 API |
| `eva/tests/corpus.json` 完整性 | 153 条固定语料、17 类、唯一 ID 与明确期望；含明确播放/暂停及已暂停场景 | 已生成并解析；未运行真实模型 |

当前登记测试直接比较 `src/ipc.rs` 的完整名称清单与规范目录：224 个 command、150 个 query、25 个订阅主题，共 399/399 登记，未知名称为空。规范目录有 224 项，其中 user/advanced 分母为 183、内部桥接为 41；126 个严格语义动作覆盖别名和复合计划。183 项的适配元数据没有 `unimplemented`。这些数字证明名称归属和代码适配存在；设备完成、真实模型选择与每项实际后置条件的验收须分别记录，不能据此宣称 183/183 真机通过。

`unlock_monitor` 明确标为 `policy_requires_local_auth`，受锁定的桌面不能靠语音解除认证；它仍保留在登记清单，不借此缩小分母或冒充声控可用。后端/构建不适用项另给实际原因。

像素测试 `eva_overlay_renders_persists_and_releases_without_a_window` 使用真正的 GLES 渲染和回读，不创建普通窗口；断言可见像素出现、静止帧保持和取消后消失。最终 15 项日志为 `/tmp/jwm-eva-frozen-ui.log`；另外从相同冻结 test artifact 直接运行两项 GPU 用例，保存并实际查看了 `artifacts/eva/final-confirmation.png` 与 `final-overlay.png`。确认画面有完整八项清单与按键提示，麦克风画面有明确开启文字、电平和退出入口；命令、环境、源码/二进制/图片 SHA 在 `artifacts/eva/final-visual-report.json`。只有强制 `JWM_REQUIRE_HEADLESS_GL=1` 且命令成功，才作为真实 GPU 测试证据，不把跳过当通过。

真实桌面 IPC 的最终七步为：独立实例/显示器上下文；真 Eva 与真 WM 的 Idle 资源和干净退出；带冻结摘要切 tag 3；改 scrolling 并核对后状态；取得原生剪贴板所有权回执；取消后迟到动作与旧 heartbeat 被拒绝；重复请求只返回历史回执。最终 JWM SHA-256 为 `b132f32a4b3822536a4d0a84d463d0283829d392a20b4ea043066eadecf0ff46`，Eva 为 `219ee00959b94947774dc6d8ba724c8a7754f0428fbaf5961d204f0e94baab24`。它没有启动云端或采集真实音频。Wayland-X11 的实际错误为 `MissingExtension(NotFound { name: "DRI3", ... })`，仍需具备 DRI3 的隔离宿主补验。

最终三个真 WM 隔离运行中的 Eva Idle 基线如下；均为 debug 构建、约 2.03 秒短采样、10 ms CPU 计数粒度，不能视作 release 性能分位数。每 50 ms 检查线程，未观察到采集/播放/native PipeWire 线程；这些证据不替代实体设备实测。

| 后端 | Eva CPU（单核百分比） | RSS | 线程 | SIGTERM 至干净退出 |
|---|---:|---:|---:|---:|
| X11RB | 0.490% | 13140 KiB | 6 | 64.00 ms |
| XCB | 0.975% | 13020 KiB | 6 | 63.76 ms |
| Wayland-winit | 0.489% | 13016 KiB | 6 | 15.65 ms |

最终 native 图日志保存在 `artifacts/eva/native-audio-final.log` 与 `native-audio-48k-final.log`。`first_feedback_playback` 仅在可信时钟观察到已播放样本后发出，24 kHz 图为 256、48 kHz 图为 257 samples；普通 loop 释放队列锁后记录，入队/未知时钟/零进度不会触发。单次结果不是性能分位数。先前失败不作为通过证据。Eva/core/client 最终 Rust 1.89 全套输出归档为 `artifacts/eva/eva-msrv-tests.log`，Clippy 为 `eva-msrv-clippy.log`；对应最终客户端与 native test artifact 的 SHA 见 `eva-local-report.json`。JWM 冻结矩阵的命令、退出码和依赖源码 SHA 见 `artifacts/eva/protocol-frozen-report.json`；其中旧客户端快照保留为历史记录，最终客户端桌面证据以 `desktop-final-report.json` 为准。

新增合同回归覆盖：完整 permit 隔离跨 JWM 重启的同编号 generation；实际异步转写只补当前已提交 item 的字幕、不能更改状态或落盘；已核验媒体结果优先于旧的进程启动读回；显式缺失配置报错、客户端与 daemon 配置摘要不一致时禁止新工作；usage 文件在读取前检查 inode、权限和大小；超过一分钟的未知执行回执仍按有界轮转公平核验，绝不重发修改。撤销展示入口依据最新可逆计划、实例与期限，单轴最大化等无法完整表达的原态不冒称可逆，新修改仍阻止跳过它撤销旧计划。

输入准入另外按成功 append 的 PCM 累计样本建立来源范围：clear 只推进本轮 floor，不重置会话累计量；服务端起声偏移必须落在本轮已上传范围，映射到原生帧时间后才能绑定对象。未验证时间不授予窗口绑定或迟到宽限，跨 IPC 重连前的采集不得晚绑定到新连接；样本长度硬上限不因迟到 VAD 延长。这依据官方 [`audio_start_ms` 的累计偏移定义](https://developers.openai.com/api/reference/resources/realtime/server-events)。

每次规划/发现/修复显式构造 [`response.input`](https://developers.openai.com/api/reference/resources/realtime/client-events)，仅引用已归属的用户音频、有限的已核验事实和本轮上下文；额外音频 B 不会因默认 conversation 已存在而混入 A 的计划。反馈也只读取本机结果，禁用工具。真实 loopback WebSocket 回归验证了 A/B 隔离及发现/修复链，未归属 B 产生的 ASR 费用仍补登记一次。取消后迟到的当前音频转写回执仍关联原预留并只结算一次，不重新授权执行或更新字幕。Doctor 的费用入口只读持久快照，区分已结算、在途、未知估算并列明模型/价格版本；没有账本或读取失败不伪造账户费用为零，官方账户总账仍需用户查看。

## 逐项要求与证据映射

| 方案条目 | 必须成立的要求 | 当前实现/检查入口 | 尚需证明的范围 |
|---|---|---|---|
| §1 基线与范围 | 独立 Rust 服务、快捷键激活、Idle 不采集 | `eva/`；`src/config.rs` 的 `eva_toggle`；三个真实隔离 WM 的 Idle/退出实测 | 实体用户会话与设备；API 实测 |
| §2 协议基础 | 使用本地逐行 JSON；不让模型绕过本地服务端条件 | `crates/jwm_ipc_client`、`crates/jwm_ipc_transport`、JWM 跨进程取消与三类隔离桌面 IPC | 实体用户会话长时并发和网络故障组合 |
| §3 模块隔离 | 音频与 WebSocket 不进入 JWM 主循环；单实例且单修改计划 | native 独立音频 loop；真实 daemon 程序测试验证 0600 socket、重复进程拒绝、Idle 无音频线程、SIGTERM 清理 | 实体设备和图形登录会话异常退出 |
| §4 原生快捷键 | Alt+Ctrl+E，非 repeatable；已有绑定和 chord 优先 | `Config::default_eva_toggle_key`、`eva_toggle_fallback_keys`、配置测试 | X11/Wayland 真键盘输入 |
| §4 Esc 作用域 | 活跃租约期间才抓 Esc；不长期抓整个键盘；取消先撤销许可 | `eva_extend_key_grabs`、`eva_sync_ui`、`eva_cancel` | X11 抢占 grab 失败、所有锁屏途径的实机检查 |
| §4 确认 | 本地 Alt+Ctrl+Enter 确认冻结范围；模型不能伪造许可 | `eva_confirm`、本地冻结 display_items；8 项清单；Alt+Ctrl+D 详情/PgUp/PgDn，全部详情页查看后才准入 | 真键盘确认/超时/取消；布局、分页完整性和本地输入测试已通过；见本地运行记录 |
| §5 状态机 | 旧 epoch/turn/response 不得重新开麦、播放、派发 | core session/realtime；daemon Update 比对完整 AssistantPermit；WebSocket 回放与跨 WM 实例回归 | 官方服务真实时序、设备停止与模型回执竞争 |
| §5 历史回执 | 旧动作/费用回执可结算，不授予当前执行权 | `ledger.rs`、`usage.rs` | 超时迟到回执与崩溃恢复集成 |
| §6 音频格式 | 24 kHz、mono、PCM16LE、20 ms；不是 WAV header | `eva/src/audio.rs` 的 native PipeWire stream/S16LE 协商 | 真实 PipeWire 设备输出字节、重采样和稳定设备 ID；禁止 monitor-source 自动回退 |
| §6 有界缓冲 | 2 s 连接缓冲、20 s 单句、2 s 播放积压；丢块整句作废 | core session 限时；audio 队列边界 | 网络堵塞和设备失效的实机注入；不能按 suffix 执行 |
| §6 半双工 | 规划/执行/反馈停采；反馈完成+尾声后才跟进 | session 状态；Capture/Playback Drop join 清理、PipeWire drained 回执 | 真麦克风占用、扬声器回录、实际播放结束时点 |
| §6 FollowUp | 5 s 从本地播放结束计时；截止前起声有最多 1500 ms 确认宽限 | core session 截止测试 | 与采集时间戳和 server VAD 接线后的迟到事件实测 |
| §7 API 生命周期 | session.created → update → updated 验证 → 音频；VAD 不自动生成回复 | provider 配置构造与事件适配 | 指定账号/模型是否可用、官方实际响应与配置确认 |
| §8 完整计划准入 | 仅 completed response.done；恰好一次完整 eva_submit_plan；不执行 delta | core realtime reducer、严格 validator；loopback WebSocket 真 pipeline 覆盖 discovery→final plan→function output、一次修复、迟到/重复调用与反馈禁用工具 | 官方 API 原始事件结构和真实账号可用性 |
| §8 两阶段响应 | 本地真实结果生成 function_call_output；反馈禁用 tools | provider 响应构造 | 真 API 音频反馈不得宣称未核验动作成功 |
| §9 语义与目标 | 1-based tag；短期 w/m 引用；窗口与显示器实例 token；布局归一化 | JWM CLOCK_MONOTONIC 焦点历史、native 来源时间戳、起声绑定和 A→B→A 回归；短 ref 包含 slotmap generation，关闭替换后不复用 | 实体起声时刻与模型等待期间切焦点的声学实测 |
| §10 全量登记 | 所有运行时 command/query 有规范归属；内部桥接不当作模型写入 | 完整源名称清单比较实跑 399/399；`capabilities` CLI 另比较当前连接 WM 的真实列表 | 用户安装实例运行时与后续新增名字的持续 CI 检查 |
| §10 全量用户能力 | 每个规范能力有 schema、定向/guard适配、完成契约和验收 | core `semantic_actions`、JWM `eva_dispatch` | 不能靠 `implemented` 标签代替每项后置条件测试；设备/捕获/配置/会话逐项真机验收 |
| §10 应用启动 | application_id→本地 argv；不把模型字符串拼成 shell | registry/protocol 与 daemon 本地映射 | 安装配置 allowlist、参数/URL 边界和目标窗口出现的验证 |
| §11 一致上下文 | 同主循环回合 snapshot；wm_instance 和 slotmap generation | `eva_execution_context`、generation token/ref、进程 PID/startticks；原生 ID 重用及跨 turn 短 ref 不复用测试 | 真实显示器热插拔和用户桌面重启矩阵 |
| §11 guarded_execute | 同回合校验实例/许可/锁屏/条件后派发；容量先保留 | `eva_guarded_execute` 与服务端测试 | 真多客户端取消与延时乱序输入 |
| §11 去重 | ID+摘要+完整请求；保留期内 at-most-once；未知异步项不自动重发 | 服务端 operation 表；`get_execution_status` | 1024 上限、10 min 保留、异步结果未知和跨重启边界长时验收 |
| §12 多步结果 | accepted/succeeded/partial/unknown 区分；每步后置条件 | 真实串行 executor 跨 Unix socket 注入第二步错误、上下文失败与丢回执；已完成步骤保留、剩余停止、不重发未知修改；设备/捕获/媒体用真实关联读回 | 实体设备、文件导出、媒体实际播放及反馈与每项后状态一致 |
| §13 取消 | 取消先关本地准入；未派发不再发；已派发保留真实结果 | 服务端 permit/epoch；session cancel | 正在播放/正在派发/异步 accepted 三类时序实机测试 |
| §13 重试 | 查询有限重试；toggle/spawn/close 结果未知不盲重放 | operation status lookup 与 core ledger | 断线恢复实际执行路径、每个非幂等适配 |
| §13 撤销 | 20 个/10 min；逆序补偿；冲突或实例变化拒绝 | CLI/语音 `undo_last_plan`、daemon executor、core ledger；JWM 同主循环 expected_state 比较后补偿，冲突测试零修改 | 真实用户手改布局、标签和窗口后撤销 |
| §14 权限与凭据 | 用户私有 socket；同 UID；Secret Service/0600 file；不泄密 | daemon/store/config 路径 | symlink/同用户/权限错误/密钥诊断脱敏的系统验收 |
| §14 隐私 | raw audio 不落盘；默认不存转写、剪贴板正文或完整标题 | config defaults、日志/序列化代码审查 | 实际文件与日志抽查；取消/锁屏后的缓冲清除 |
| §14 费用 | response ID 去重；输入/缓存/输出分项；在途预留；未知不清零 | core UsageMeter、`eva/src/store.rs` | 当前价格配置核对、跨日期/时区边界、实际 usage 回执对账 |
| §15 浮层 | 独立生命周期、不抢焦点、下方居中、状态文字/电平/8 项确认清单/长值完整详情 | `eva_overlay.rs`；X11/udev GL；nested MemoryRenderBuffer | 各后端 DPI/多屏/热插拔；X11 compositor 关闭时拒绝启动而非无提示开麦 |
| §15 失联 | 释放 Esc；不把心跳失联说成麦克风已释放 | JWM error visible deadline；overlay “状态待确认” | daemon watchdog 确认实际关采集 |
| §16 配置/服务/CLI | 严格未知字段、用户服务、daemon/toggle/cancel/status/capabilities/doctor/dry-run | 实际严格 TOML loader、安装升级回归、graphical-session notify/watchdog service；真实控制 socket 与串行 executor 集成 | 用户会话 enable/start/登出，凭据存储与所有 CLI 的真实桌面联用 |
| §17 构建隔离/CI | 核心构建不引入网络音频 SDK；独立 Eva 构建；无凭据测试不冒充 E2E | 默认/精简服务端矩阵、core/daemon/transport/安装离线检查、必须执行的私有 native 图、显式 opt-in API handshake gate | 远端 CI 执行结果；handshake 仍不等于语音或设备验收 |
| §18 诊断 | 归一化错误码、激活至资源释放时间线、冷/暖分开 | core/session/provider与daemon日志 | 真实诊断输出及性能计时报告 |
| §19 成本模型 | 价格版本可配置；缓存/非缓存、计划/修复/反馈分别结算；不把音频规模例子当完整账单 | core UsageMeter、daemon Store | 当前费率与价格版本展示、可选转写独立费用、真实账户对账 |
| §20 指标 | 语义≥95%；P50/P95延迟；Idle CPU/RSS；本地开销≤50 ms P95 | 固定语料与评测口径；三个真隔离 WM 的 debug Idle 基线已实测 | 真实语音语义、端到端分位数、release 与实体用户会话资源仍待测 |
| §21 演示 | 连续 tag→layout→move；Esc阻止迟到；Idle关麦；模型等待切焦点不误改 | 下面七步清单 | 真实 API + 两类桌面实测后才能签收 |
| §22 后续扩展 | future_wake_word=false；无 idle 关键词采集 | 配置默认与拒绝不支持的开启方式 | P4 不属于当前默认可启用能力 |

确认清单从 JWM 冻结的本地对象与参数构造；服务端不以模型摘要授权。普通清单最多显示 8 项，标题仅作可缩略的辅助文本；长配置键/值完整进入固定字号的分页详情。Alt+Ctrl+D 打开/关闭详情，PageUp/PageDown 翻页，所有详情页查看后才允许 Alt+Ctrl+Enter 确认。全局布局仅在冻结内容、字体或视口改变时重建；翻页与心跳复用缓存。锁屏/取消/新范围会撤销许可或清除浏览状态。

`open_media` 不以进程启动当作播放成功：启动只保留 `accepted` 与 `player_process_spawned`。最终通过 native player 关联同一 operation、WM、launch sequence、规范资源 URI、模式与 owner/PID，再要求本次 mpv 命令 ACK、player request ID 和新 playlist entry ID；直接播放还要求该资源的实际 `Playing`。普通状态轮询、已有同 URI 队列条目或无权威基线不能结算该操作。重复 operation 只返回原回执，丢回执时保留 `unknown` 而不再次 loadfile。缺少同次构建的 bridge/player 时不冒称完成；硬件声音和实际用户媒体仍需实测。

明确的“播放”和“暂停”经 `media_control/play`、`media_control/pause` 调用幂等 MPRIS setter，等待同一播放器的实际状态读回；已有暂停缓存不会被当作新命令完成证据。`play_pause` 仅用于用户明确要求切换状态。

## 必须补齐的真实运行验收

按方案 §21 的原步骤，不替换为输入文字的简化演示：

1. Alt+Ctrl+E 激活；Connecting 后出现 Listening 和开麦电平。
2. 说“切到第三个 tag”；核对激活显示器的可见集合。
3. 跟进说“换成 scrolling 布局”；核对同一工作区。
4. 说“把这个窗口移到第八个 tag，然后切过去”；核对同一 target_token。
5. 模型等待期间按 Esc；观察许可撤销后迟到计划无法修改桌面。
6. 结束后用 PipeWire/进程工具确认麦克风流释放；不只看 UI 文案。
7. 等待模型时切换应用；核对动作不会误改新焦点窗口。

每一步分别记录 X11 与 Wayland、实际模型 ID、配置摘要、是否冷连接、句末时间、最后一步核验时间、结果状态与失败原因。无 API key 时保持“待实测”。

还需对真实设备覆盖：拔掉输入设备、断网/429/认证失败、连接缓存溢出、播放设备堵塞、锁屏、daemon 被终止、JWM 重启、显示器拔插、修改目标状态后撤销、录制结束文件完整性、应用进程与窗口出现的区别。每种异常都必须检查不会误改对象且采集/播放按规定终止。

## 语料评测规则

`eva/tests/corpus.json` 固定包含场景、原话、期望意图、必要时的规范动作和禁止副作用。文件中的 `expected` 是人工预期，不是模拟返回。读查询可以用只读查询或回答路径，但不得借机聚焦窗口。否定、歧义、未授权确认、ID 重用和取消用例采用零误操作门槛，不允许用普通样例的成功率抵消。

新增真实评测报告应另存 JSON/CSV，包含 case_id、模型、轮次、实际完整计划、验证结果、真实后置条件、延迟和费用。不要把凭据、原始音频或完整桌面隐私数据写入结果文件。分别报告普通语义成功率、确定性隔离/取消故障结果、冷/暖延迟分位数，以及未执行和跳过的原因。

## A01–A31 故障验收追踪

“自动化入口”列只指出相应测试/实现位置；结果仍以本文件上方实际命令记录为准。涉及麦克风、真实 WM 或云端的部分须独立实测。

| 编号 | 自动化入口/已有证据 | 必需的补充验收 |
|---|---|---|
| A01 | core `idle_and_processing_release_capture_and_old_events_cannot_reopen`；native 合成图确认 stop 后采集节点消失 | 实体设备关麦与 Idle 零上传 |
| A02 | `eva_hotkey_is_native_non_repeating_and_respects_custom_keys`、`eva_cancel_grabs_exist_only_while_active_and_escape_revokes_first` | 并发 CLI/服务启动单实例、真键盘长按 |
| A03 | core tag-number/mask 测试；服务端定向动作 | 真实第 1/3/8 tag 的窗口/显示器状态 |
| A04 | core strict schema 与 tag collection tests | 三种非法语音输入不产生动作 |
| A05 | core layout schema/normalization；语料 layout 分类 | 真模型布局语义与后状态 |
| A06 | 语料 negation 12 条 | 完整真实语音输入后零动作 |
| A07 | 语料 correction；realtime 仅 response.done 授权 | 流中出现错误目标后修正时不先执行 |
| A08 | JWM `directed_window_move_keeps_identity_when_focus_changes`、`explicit_focus_condition_rejects_changed_selection` | 采集时刻焦点历史与真实异步模型延时 |
| A09 | JWM `recycled_native_window_id_does_not_retarget_old_token` | 两后端窗口退出与 ID 重用 |
| A10 | JWM `cancelled_permit_and_old_heartbeat_cannot_reactivate` | 真 socket 延迟完整计划 |
| A11 | core `arguments_done_and_cancelled_response_never_authorize` | 官方取消事件录制回放 |
| A12 | core `one_turn_has_one_claim_even_different_call_ids`、`multiple_tool_calls_fail_as_whole_response`；JWM重试回执测试 | 多进程相同 operation 请求竞态 |
| A13 | 服务端同 operation 完整请求/摘要比对 | 真 socket 不同摘要重复请求 |
| A14 | core ledger 保留逐步状态 | daemon 第二步错误的 partial 反馈与不重放第一步 |
| A15 | core crash recovery unknown 不重放 | 写入 socket 后杀连接，非幂等真实操作核验 |
| A16 | 服务端 accepted 与异步回执路径 | 截图文件/录制结束不提前标 succeeded |
| A17 | core response gate；反馈 response tools=[] | 反馈阶段注入 function call 仍零动作 |
| A18 | 服务端 lease/session lock 检查、精确维护 deadline | 锁屏/登出/kill daemon 同时释放麦克风和 Esc |
| A19 | core `undo_is_reverse_order_and_rejects_manual_changes` | 真实手改布局和窗口后撤销拒绝 |
| A20 | audio overflow 返回错误与 capture Drop | 真实 USB 拔出、堵塞网络/缓冲溢出 |
| A21 | core `near_deadline_speech_survives_remote_vad_delay`、`noise_candidate_has_bounded_grace` | 4.9 s采集时间与5.2 s VAD事件接线 |
| A22 | 独立于 DND/toast/bar 的 renderer；X11 fullscreen composition blocker；GL像素测试 | DND/隐藏bar/全屏真桌面开麦标志和退出输入 |
| A23 | monitor slotmap token；activation monitor保留/有效输出回退 | 两后端热插拔时拒绝旧目标 |
| A24 | 配置现有绑定/反序modifiers/chord leader测试 | 用户自定义配置实际加载诊断 |
| A25 | core cached usage 不双计、重复settlement去重、未知reserve保留 | 真 API usage迟到/结构修复费用与历史操作回执 |
| A26 | JWM `internal_writes_and_extra_fields_are_not_guarded_actions` | 普通语音不得调用内部写入伪造状态 |
| A27 | 语料 title_injection/policy_boundary | 含恶意标题的真实模型计划不增加权限 |
| A28 | core `crash_recovery_does_not_replay_or_claim_success`；wm_instance token | 真实重启JWM/daemon默认Idle且无未知重放 |
| A29 | JWM `retry_after_cancel_returns_receipt_and_never_replays`、旧heartbeat测试 | 真IPC人为延迟取消通知，旧许可仍被拒绝 |
| A30 | JWM `capacity_is_reserved_before_any_side_effect` | in_flight超过TTL、容量满长期运行 |
| A31 | core feedback_completed状态边界；daemon反馈分支 | 文本/本地音/远程失败各路径实测不卡Responding |

## 工程性能目标的报告口径

以下尚无完整实机分位数；合成图单次停止/播放测量不能代表目标 P95：激活提示 P95≤100 ms；取消播放/新派发 P95≤100 ms；退出释放设备 P95≤300 ms；计划就绪后的本地验证派发 P95≤50 ms；暖会话句末到真实完成 P50≤1.5 s/P95≤3 s；代表语料成功率≥95%；确定性目标隔离与取消全部通过。本次提供了隔离真 WM 的 debug Idle CPU/RSS 基线，release 与实体用户会话仍需补测。冷连接、网络超时、设备故障与未知结果单列。

## 音频与 provider 补充审计

当前音频实现使用 native PipeWire，独立 loop 线程通过固定 S16LE/24 kHz/mono POD 协商格式；不再依赖 pw-record/pw-play 的文件格式或子进程生命周期。采集主动请求 MetaHeader，优先使用来源 PTS；也支持 native buffer cycle + 图时钟 + 设备/滤镜延迟 + 重采样积压推导首样本时间。来源未能验证时 Frame.timestamp_verified=false，窗口起声绑定必须保守拒绝猜测。实时回调只复制到预分配 SPSC ring，普通 loop 线程负责分帧和事件投递；丢块、时间跳变或积压作废整句。Capture::check 把设备/丢块/格式故障立即传播，不等队列里的错误排到头。

播放将应用队列和已交给 PipeWire 但未播放的样本合并限制为 2 s；原生 pw_buffer.size 显式以帧为单位。成功的 finish 需要明确请求 drain 并收到 drained 回执。播放位置依据新鲜图时钟，扣除 native queued、buffered 和换算后的设备/图延迟；不把提交字节当成已播放样本。取消时先停止本地流并冻结时钟快照，再按每个 item 的源样本范围保留完整已听条目、发送 conversation.item.truncate 截断部分条目并删除未听条目；时钟缺失或陈旧时保守删除未证明已完整播放的条目。历史修复失败则结束会话，不带未听历史继续。

构建与实际加载的客户端 libpipewire/libspa 需要 ≥1.2，安装器可提供私有前缀与运行库路径；旧服务端可连接但若不给充分时钟证据，不推断对象或播放位置。隔离测试使用真实 Dummy 单调驱动运行同步 audiotestsrc（live=false），避免把不填 Clock 的异步虚拟源当作有效时钟。

provider 在 session.created 前拒绝配置，在 session.updated 完整核对 PCM、voice、VAD 阈值/填充/静音、自动响应开关与转写配置之后才允许 append。PCM 空包/奇数字节/超长包在发送前拒绝，网络/API错误只输出归一化错误码。真实 API 参数和设备流仍待各自集成实测。

PipeWire 时钟/队列/设备延迟是不同量，参考[官方 pw_time 说明](https://docs.pipewire.org/structpw__time.html)；会话配置回执流程参考[OpenAI Realtime 会话指南](https://developers.openai.com/api/docs/guides/realtime-conversations)。

## 2026-10-30 用户 X11 会话与真实语音联调

本轮使用用户已安装并重启的 JWM、实体麦克风和 `gpt-realtime-2.1-mini`，凭据由用户在本机输入受保护文件。未保存原始音频、凭据或完整桌面标题。初次联调先修复了 TLS provider 初始化、单个 function call 与附带 message 的解析、严格计划跨字段约束、PipeWire 首块跨度以及阻塞存储影响 watchdog 的问题；失败记录保留，不能只统计重试成功。

| 场景 | 本轮观察与用户确认 |
|---|---|
| 只读真实 API 计划 | “切到第三个 tag”生成正确计划，`execution_enabled=false`；不将其计为语音执行 |
| 语音切换 tag | 实体语音切到第三个 tag，真实 operation 后状态与用户确认一致 |
| 语音布局与撤销 | 第三个 tag 切为 scrolling；语音撤销恢复 tile；两次真实后状态与用户确认一致 |
| Esc 取消 | 说切到第五个 tag 后立即 Esc；观察到取消回 Idle，未产生该 tag 的执行记录，用户确认没有切换 |

上述首次实体语音验证使用临时 PipeWire ALSA 接管。测试后已恢复原 PulseAudio 服务和配置；输入、输出、音量、静音与声卡 profile 对照原快照一致。此步骤不作为日常激活方式，后续自动激活应直接使用桌面的现有音频服务。

本轮只证明这些基础场景；没有取得完整语料成功率、P50/P95、跨应用起声绑定、Wayland 实体设备或全部故障矩阵的实测结果。

### 快捷键自动连接现有音频服务

用户要求恢复原音频设置后仍能通过 Alt+Ctrl+E 自动开麦。现增加原生 PulseAudio 后端：真实 PulseAudio 桌面直接使用其当前输入/输出，PipeWire 桌面优先使用原生 PipeWire。探测和连接在可取消的音频线程中完成；`doctor` 只查询设备，不创建音频流。不会启停系统音频服务、修改默认设备或解除静音。显式指定的输入不存在时拒绝回退，PulseAudio monitor 不作为麦克风。

PulseAudio 采集使用本机 Unix socket、原生 timing/index 与带误差界限的墙钟到单调时钟映射；不以回调到达时刻冒充采集时间。播放尾部除 drain ACK 外还需新鲜原生时间证据，确认最后音频已离开 sink FIFO；普通部分播放位置不累加静音进度。两种后端均在取消时关闭本次 stream。

| 检查 | 实测结果 |
|---|---|
| Rust 1.89 Eva/core 与集成测试 | Eva 92 passed、4 个原生音频用例单独执行；core 37 passed；daemon/executor/packaging/undo 共 6 passed；真实 API opt-in 用例未在此轮重跑 |
| 全目标 Clippy | Eva/core `-D warnings` 通过 |
| 私有 PulseAudio 15.99.1 | 2/2 通过；测试公共后端选择、可信采集、缺失/monitor 拒绝、停止、真实播放 drain 和部分历史；250 ms 音频 drain 276 ms，停止 10 ms，部分位置 321 ms；结束后零输入/输出流 |
| 私有 PipeWire 1.2.8 的 24/48 kHz 图 | 两种图速率各 2/2 通过，客户端保持 24 kHz；48 kHz 跳过首个不可信混合帧后恢复真实时间戳，采集释放 11 ms、200 ms 播放 drain 222 ms。新增回归验证未知启动片段不会永久污染后续完整帧 |
| 无响应音频服务 | 自有 Unix socket 接收握手但不回复；`Capture::start` 返回 0 ms，100 ms 后取消，1 ms 完成停止，连接全部关闭 |
| 用户原有 PulseAudio 实体麦克风 | 只在内存采集两秒，98 帧全部时间戳有效，未上传；停止 10 ms；原服务 PID 未改变 |
| 已安装程序诊断 | `audio_backend=pulseaudio`，输入/输出可用，`microphone_opened=false`，协议就绪；安装保留原 `eva.toml` 和凭据文件权限 |
| 快捷键与真实语音复测 | 用户确认 Ctrl+Alt+E 自动开麦并成功执行 tag 切换；观察到 connecting/listening/planning/responding 后回到 Idle，实际 PulseAudio 输入和输出流均归零 |

PulseAudio 隔离用例的 sine source 固定以两秒为量子，测试接收队列据此配置；生产两秒缓冲上限与溢出即拒绝的规则未放宽。PipeWire 隔离图禁用硬件发现，并检查会话管理器实际注册；两类 fixture 都不能连接用户音频图。

复测后 PulseAudio PID、默认输入和默认输出与安装前一致；快捷键只管理 Eva 自己的音频流。
