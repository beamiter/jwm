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

## mini 唤醒、标准版连续对话升级验收

本节记录新的验收范围，不覆盖前面的历史实测结果。模型配置改为标准版 `gpt-realtime`，mini `gpt-realtime-mini` 仅用于可开关的云端唤醒监听；旧版单模型真实语音结果不能视为本次双模型或打断链路已经通过。

| 场景 | 必须观察的结果 |
|---|---|
| 开启与关闭监听 | 自定义 `eva_wake_toggle` 和 `jwm-eva wake enable/disable` 同步状态；开启后明确显示麦克风活动，关闭后原生采集流消失且零上传 |
| 待机模型隔离 | 只有 mini 接收待机音频，工具仅用于判断唤醒；普通话、背景聊天、电视声和恶意音频中的桌面指令不直接执行 |
| 唤醒后切换 | “hi eva”后先关闭 mini 流再打开标准版；等 Listening 后的新指令才允许规划；记录切换延迟，唤醒句内的后续命令不计为已执行 |
| 回到待机 | 标准会话超时后恢复 mini；明确关闭监听后不自动重启；锁屏停止监听，解锁后不自行开麦 |
| 连续对话 | 30 秒跟进窗口中的新语音关联新轮次；澄清回复、普通聊天和中英混合指令保持历史一致，确认与执行结果只能来自本地 |
| 模糊指令 | 两个近似标题、跨屏同名窗口、无对应目标时先澄清；清晰匹配后仍以稳定窗口引用执行，焦点变化不重定向 |
| 长连续指令 | 单句 120 秒范围、9–32 步顺序动作、不超过配置的规划轮数；超过上限时明确反馈，不静默截断后执行不完整计划 |
| 理解期间打断 | 说“停一下，改成……”后旧计划不派发，取消或迟到 response.done 不恢复执行；新语音保持真实来源时序 |
| 执行期间打断 | 停止尚未派发步骤；已完成/已提交步骤以真实回执报告，不把取消当作撤销，不自动重放结果未知动作 |
| 回复期间打断 | 本地播放及时停止；按实际播放时钟截断历史，新一轮不把未听内容当作用户已知；远端取消失败仍不恢复旧音频 |
| 回声与噪声 | 耳机与扬声器分别测试，背景音乐、电视、键盘噪声不连续误打断或自我唤醒；报告误触发率和音频环境 |
| 故障与预算 | 断网、无权限模型、设备拔出、日预算不足时有可读状态，麦克风按状态释放；两模型费用按各自费率记录，同一天共享额度 |
| 能力覆盖 | 比较实际 `capabilities` 与 JWM 命令目录；逐类验证窗口、工作区、布局、显示器、媒体、设备、桌面界面、配置与会话操作。登记覆盖不能代替真实语义执行结果 |

自动化检查应分别报告配置加载、唤醒事件关联、取消与轮次隔离、长计划上限和 IPC 覆盖结果。真实 OpenAI 模型、麦克风、PulseAudio/PipeWire、X11/Wayland 与网络条件下的端到端结果需要另行记录；未运行的项目标为未验证，不能根据离线测试声称达到 GPT Live 的流畅度。

本次工作区离线验证（2026-10-29）：

| 检查 | 结果 |
|---|---|
| Eva 单元与 loopback | 108 passed；包括 mini 音频来源/响应关联/独立计费、打断后的 clear 确认与重传、助手回答历史、迟到取消费用回执 |
| daemon / executor / packaging / undo 集成 | 6 passed |
| Eva core / IPC client | 39 / 17 passed |
| 原生 JWM EVA / 浮层 / 快捷键 | 35 / 8 / 2 passed；包含过期唤醒交接拒绝、待机麦克风状态及 32 步确认分页 |
| 安装脚本 | 安装、引号、旧配置保留、服务生命周期及失败路径检查通过 |
| 原生音频隔离图与真实 API | 4 个隔离音频用例、1 个双模型 API 握手用例未运行；本环境缺少音频 fixture 服务，未使用真实 API 凭据或实体麦克风，不能据此认定实机语音端到端通过 |

完整套件命令：`cargo test --locked --offline -p jwm-eva -p jwm_eva_core -p jwm_ipc_client`。本次私有 PipeWire 客户端为 1.6.10；编译使用准备好的 PipeWire/SPA 头文件与 Clang 资源路径，未修改桌面音频服务。

## 2026-10-30 连句唤醒、语义断句与任务续接验收

本节为进一步演进后的验收范围；同句唤醒的口径取代前节“等待 Listening 后另说指令”的第一轮限制。前面已记录的离线测试数量与实体语音结果保留为各自版本的历史证据，不代表本节已通过。新增检查结果需在运行后另记。

| 场景 | 必须观察的结果 |
|---|---|
| 语义断句与长停顿 | `semantic_vad`、`eagerness=auto` 下，说到一半停顿、犹豫、补充限定词或“然后……”不被固定 600 ms 静音过早切断；单句最长时限仍生效，分别记录接话延迟与误断句率 |
| 可调接话速度 | 对比 `low/medium/high/auto`；用户能通过配置选择等待程度，服务端配置回执与选择一致，不把未知值静默替换成默认值 |
| 旧配置兼容 | `server_vad` 继续使用阈值、前缀和静音设置；mini 待机固定用 server VAD；两种断句分别完成会话配置核验 |
| 输入降噪 | 近距离麦克风用 `near_field`、房间麦克风用 `far_field`、`off` 关闭输入降噪；回执与配置一致，分别记录噪声下语义识别和误打断率，不能把输入降噪等同于回声消除 |
| 同句唤醒与命令 | 连着说“hi eva，把这个窗口放到第三个桌面，再切过去”；标准版处理带真实时间戳的原始音频，窗口目标来自唤醒时起声上下文，不以 mini 生成的文字替代用户输入 |
| 唤醒交接边界 | 超长句、音频缺帧、时间戳不可信、焦点变化、显示器拔出、锁屏或 WM 重启时拒绝陈旧交接并提示重说；不能把旧目标换成新焦点 |
| 交接资源与费用 | 原始音频仅保留在有上限的内存缓冲中；交接支持最长 15 秒唤醒句，标准版接管前关闭 mini 云端连接，本地采集流保持单一所有者；取消清空缓冲；mini 判断和标准版处理分别记录模型费用 |
| 中断后继续 | 执行多步任务中说“停一下”，再说“继续剩下的”；以已核验回执确定剩余步骤，已成功、已提交或结果未知的步骤不自动重放 |
| 中断后修正 | 说“后面的改成第四个桌面”或“只继续移动窗口，别切过去”；结合本地剩余任务和新语音重做计划，仍需满足目标与确认校验 |
| 无可续接任务 | 已全部完成、只有聊天、上下文失效或 WM 重启后说“继续”；不编造剩余动作，也不沿用旧执行许可，需要明确解释或澄清 |
| 续接确认 | 涉及关闭、配置或会话恢复的剩余动作，重新生成计划后按本地规则确认；旧批准不能覆盖修改后的计划 |

建议分别在耳机、近距离扬声器和房间麦克风环境录入以上测试的成功率、误唤醒/误打断、句末到首音延迟以及取消到停止播放延迟。离线事件测试、真实 API 配置握手与完整语音端到端测试分开报告。

本轮最终离线结果（2026-10-30，基于 f5f8d596 的工作区改动）：

| 检查 | 结果 |
|---|---|
| Eva 单元与 loopback | 154 passed；同句音频单次提交、输入 item 确认/取消、早到 ASR 关联及计费、后续音频顺序、语义 VAD/降噪契约、任务续做/修正/取消、旧任务作废及未知操作别名隔离均覆盖 |
| 语料契约与 daemon/executor/packaging/undo 集成 | 8 passed |
| Eva core / IPC client | 40 / 17 passed |
| 原生 JWM EVA | 45 passed；包含 WM/会话代次隔离、独立显示器历史、起声显示器证明与固定、失效后的仅唤醒回退 |
| 安装与格式 | 安装器检查、`cargo fmt --all -- --check`、`git diff --check` 通过 |
| 原生音频图与真实云端 | 4 个原生音频隔离图用例、1 个实时 API 用例未运行；不代表真实麦克风、扬声器回声或模型语义表现已验收 |

合计 264 项自动化用例通过。任务和音频交接仅保存在当前进程的有界内存中，未新增录音或转写持久化。手动端到端矩阵仍待使用实际安装配置和真实设备逐项验证。

## 语音结束会话、真实任务进度与唤醒故障恢复验收

本节为下一轮演进的待验收范围；前面的测试数量保留为对应版本的历史结果。以下项目需分别报告离线事件验证与真实语音结果，不以新增界面或工具字段代替端到端验收。

| 场景 | 必须观察的结果 |
|---|---|
| 语音结束当前对话 | 明确要求结束聊天时，标准版会话释放采集、播放及待续任务后结束；之前已开启的 mini 唤醒可恢复，未开启时保持 Idle |
| 语音停止监听 | 在标准版对话中明确要求关闭麦克风或停止监听时，同时关闭当前对话和 mini 待机；等候超过跟进窗口或再次说唤醒词都不重新开麦，须显式重新开启 |
| 取消任务与结束对话分开 | “取消这件事／不用做了”只清除待续任务，仍可继续聊天；“停一下／等等”保留任务；不能把这些话自动解释成关闭整个语音服务 |
| 模糊结束表达 | 引用、否定、“别结束”“等我一下”或复述示例时不误关麦；模型生成的普通回复、窗口标题和工具结果不能触发会话生命周期命令；dry-run 只报告意图，不结束实际会话 |
| 结束期间资源顺序 | 原生结束请求发出前，当前 Capture/Playback 和队列已释放，不等待云端告别；迟到播放、输入事件及旧执行计划不恢复；已经提交的操作仍按本地回执结算，不将关麦当作撤销；本地手动开启的新唤醒设置优先于旧停止请求，提示与实际状态一致 |
| 真实进度 | 长计划按本地已派发步骤、关联回执和后置条件显示进度；`accepted`、仍在执行、结果未知与已核验成功分开，不能因模型文字宣称完成就增加成功数 |
| 打断与续做进度 | 打断后进度保留实际已完成/未知结果；续做重新规划的步骤不重算已完成操作，旧计划进度不能覆盖新轮次 |
| 暂时故障恢复 | 断网、设备暂时不可用或服务暂时错误时关闭采集，按 reconnect_ms 起始的指数退避及 ±20% 抖动重试，最长等待 60 秒；连续失败 8 次后停止（最多自动重试 7 次），稳定监听 5 秒后计数重置；状态明确显示等待与停止，恢复后重新核验配置、来源和权限 |
| 永久故障停止 | 无效凭据、模型无权限、额度或日预算不足时停止自动重试及待机上传；修正后需明确重新开启，不能静默循环开麦 |
| 限流冷却 | 限流至少冷却 30 秒，避免快速重复连接；取消监听、锁屏或新来源代次会撤销旧重试，不得由过期定时器重启 |

手动测试应同时检查浮层、`jwm-eva status --json`、原生音频输入/输出流及本地账本；不能只凭一句“已关闭”或进度文字判定关麦和完成。

本轮最终离线验证（2026-10-30，语音收尾、执行进度与待机恢复）：

| 检查 | 结果 |
|---|---|
| Eva 单元与 loopback | 182 passed；含严格会话意图、释放音频先于原生结束、本地取消抑制迟到收尾、进度轮次隔离、错误分类、恢复退避与取消锁存 |
| 语料与 daemon/executor/packaging/undo 集成 | 8 passed；执行器真实 Unix socket 用例同时核对 accepted/unknown 不计入完成数 |
| Eva core / IPC client | 40 / 17 passed |
| 原生 JWM EVA / 浮层 | 51 / 16 passed；含精确会话许可、停止监听代次竞态、显式重新启用、过期自动关闭拒绝、进度计数和错误卡期限 |
| 安装与格式 | 安装器、格式与差异检查通过 |
| 未执行项目 | 4 个原生音频隔离图用例及 1 个真实 API 握手用例；未以此声称真实语音端到端验收通过 |

合计 314 项自动化用例通过。另修复构建版本诊断：当前分支 ref 或 packed refs 变化会触发更新，避免提交后仍显示旧源码版本。真实设备下的收尾延迟、背景噪声和断网恢复体验仍按本节人工矩阵另行验收。

## 协议版本诊断、所选配置云端握手与配置迁移验收

本节记录本轮新增验收范围，前面的通过数量属于各自版本；未运行真实 API 时，不把离线握手模拟或新增诊断报告计为 live 通过。默认 doctor、显式 `doctor --live`、开发 opt-in API 测试和完整语音验收分别记录。

| 场景 | 必须观察的结果 |
|---|---|
| 附加协议版本 | 命令 schema、执行上下文和助手状态均声明 `assistant_protocol`；当前 `revision=1`，受保护桌面请求仍为 `protocol_version=1`，已有协议字段不被替换 |
| 所需特性校验 | `guarded_execution_v1`、`wake_source_identity_v1`、`wake_onset_monitor_history_v1`、`voice_session_finish_v1`、`verified_task_progress_v1` 均存在；缺失对象、版本、所需特性或格式错误时，诊断明确提示更新对应 JWM/Eva，而不是仅凭命令名判断兼容 |
| 执行与唤醒特性分组 | 标准会话要求 guarded_execution_v1、voice_session_finish_v1、verified_task_progress_v1；唤醒另需两个 wake 特性。执行特性齐全但唤醒特性不足且未配置启动监听时，分别报告标准协议就绪与唤醒警告，不虚报完整唤醒可用 |
| 前后兼容边界 | 新增未知特性、附加元数据或更高 revision 在已有所需特性满足时可接受；重复特性、非字符串特性或非法版本不被误判为有效 |
| 支持与当前可用分开 | 关闭浮层、锁屏或退出会话时仍报告实现支持的协议特性；`standby_blocked_reason` 分别反映 overlay_unavailable、session_locked、session_exiting，修复建议针对实际阻止原因 |
| 默认 doctor | 只检查本地配置、凭据可读取性、设备与 IPC 并读取费用快照；无麦克风采集、云端会话、推理请求或桌面修改 |
| 所选配置 live 检查 | 显式指定 `--config` 和 `doctor --live` 后，检查的对话/mini 模型、语音、断句和降噪来自该配置及对应凭据来源；报告按角色和模型给出连接/配置阶段、结果及耗时 |
| live 资源与权限 | 云端握手不创建采集/播放流，不上传音频，不提交 response.create，不执行桌面动作；标准版和 mini 都收到匹配配置回执才分别通过，握手成功不代表语音端到端通过 |
| live 失败诊断 | 无凭据时两个模型均报告 credential_unavailable 且不调用 API；无效 key、模型无权限、限流、配置不匹配和超时给出脱敏错误及具体建议，输出不得包含凭据或远端任意错误原文 |
| 迁移只检查 | `config migrate` 和 `--check` 只读；配置原始字节、权限和内容不变，不生成备份，不读取 API key 或调用云端；`--config` 精确选择待检查文件 |
| 已识别旧版写入 | 只有识别为旧版内置模型/费率/交互组合时 `--write` 才执行；备份与输入原始字节相同且权限 0600，写入采用原子替换并保留原文件权限 |
| 自定义设置保留 | 凭据来源与路径、应用登记、隐私、预算及用户自定义值不被猜测覆盖；自定义模型/费率/交互组合报告 manual_review，不自动写入 |
| 无需迁移与写入失败 | current 配置再次检查或写入保持无变更；备份或原子写入失败时保留可恢复原配置，错误清楚可读；报告只列字段名与文件路径，不输出配置中的潜在敏感值 |
| 安装与生效 | 安装器仍保留旧配置并给出检查/迁移提示；写入后重启 Eva，选用的新配置指纹、模型和费率与 doctor 一致；JWM 协议升级需使用新构建会话，不把只重启 Eva 当作 compositor 已更新 |

手动实测报告应记录所选配置路径、两模型名称、协议 revision/所需特性和失败阶段，不记录 key、完整配置或用户音频。真实 API 握手成功后仍需按前节矩阵验证唤醒交接、语义理解、播放、打断和执行结果。

本轮最终离线验证（2026-10-30，协议兼容、只读诊断与配置升级）：

| 检查 | 结果 |
|---|---|
| Eva 单元与 loopback | 204 passed；包括标准/mini 真实设置工厂复用、无音频/无推理握手、超时取消、迁移识别与精确备份、格式保留、权限及已检测的文件变化、诊断实例隔离 |
| CLI | 2 passed；默认不启用云端或写入，配置无效时在任何探测前停止 |
| 语料、daemon/executor/packaging/undo 集成 | 8 passed |
| Eva core / IPC client | 43 / 17 passed |
| 原生 JWM EVA | 53 passed；三个查询声明同一附加协议，支持能力与运行限制独立 |
| 安装与格式 | 旧配置字节、有效及悬空符号链接均保留；迁移提示不执行程序；安装器与格式/差异检查通过 |
| 未执行项目 | 4 个原生音频隔离图用例及 1 个真实 API 握手用例未运行；没有借助本次检查宣称真实设备语音体验通过 |

合计 327 项自动化用例通过。`config migrate --check` 不写入，`--write` 仅适用于关闭其他编辑器/写入者后，报告明确 `concurrent_writes_supported=false`。`doctor --live` 必须显式调用，使用所选配置和凭据来源；本次没有运行真实 API 调用。

## 浮层层级、真实电平与结果反馈界面验收

本节记录界面演进的待验收范围。渲染截图、离线状态回放和真实语音操作分别记录；合成状态的截图只能证明布局与文字呈现，不能证明真实采集、模型理解或桌面执行已经通过。现有确认卡的完整审阅、分页与本地批准要求保持不变。

| 场景 | 必须观察的结果 |
|---|---|
| 紧凑待机与活动卡 | mini 待机使用紧凑唤醒提示，标准对话展开状态／结果、内容、麦克风与操作层级；连接、暂停和退出时提示与实际会话状态一致，不把待机伪装成正在执行 |
| 结果含义 | 本地核验成功、部分完成、失败、未知、澄清、无动作和取消分别呈现；澄清问题优先可读，部分完成说明范围，未知结果不显示全部成功；继续说话阶段保留相应结果 |
| 真实麦克风电平 | 安静输入电平为零或实际底噪，不生成模拟波形；采集暂停时不显示活动电平；状态无法证明时明确待确认，不能因租约超时或错误卡出现就宣称已释放设备 |
| 任务进度分开 | 单独呈现核验完成数与总步数，准备、执行、核验、部分完成及未知区分清楚；与麦克风电平不共用含义，accepted 或模型文字不能推进成功数 |
| 结果与轮次隔离 | 新一轮开始后旧 result_status、进度或错误元数据不能覆盖新轮次；同一轮继续说话时，不因心跳、电平或字幕更新丢失结果或重置倒计时 |
| 依故障提供操作 | 可重试错误提供 Alt+Ctrl+R；retryable=false 时文字和实际按键绑定都移除重试，保留具体建议与诊断；不能只隐藏文字但仍允许过期重试 |
| 错误可读性与重新激活 | 普通会话错误卡保留 6 秒；重试或新许可清除旧错误期限，旧错误计时不能结束后来新开的会话；语音反馈失败时保留实际桌面结果与文字反馈 |
| 费用、权限与未知结果 | 凭据、模型、配置、额度或预算错误引导先处理原因；结果未知的已提交动作不因界面重试入口而自动重发；诊断只打开本地检查，不擅自开启 live API |
| 确认完整审阅 | 长参数和最多 32 步计划仍按冻结内容显示；详情页未全部看完不能批准，Esc 可取消；紧凑屏幕、主题切换或新结果样式不缩减必要信息和本地确认 |
| 字幕与长文本 | 默认无输入字幕；显式开启后与结果分区，长中文、混合英文和多行问题不遮挡麦克风状态或操作；窗口标题只用于目标辨认，不能成为可执行 UI 指令 |
| 跨后端与尺寸 | X11、Wayland 原生和嵌套路径使用一致的层级、含义、真实电平与进度；小屏和大屏、深浅主题分别检查裁切、换行、对比及文字密度 |
| 动效与安静状态 | 遵循已有减弱动效设置；过渡不能伪造录音信号或完成比例，也不能拖延本地取消、关麦及重要状态变化 |

人工验收应把状态卡截图与当时的 `status --json`、原生输入／输出流和真实执行回执对照；逐项区分“布局已看过”“状态接线已验证”和“语音任务端到端成功”。没有实际设备或 API 的项目继续标为未运行。

本轮实际验证记录（2026-10-30，浮层与结果反馈）：

| 检查 | 结果 |
|---|---|
| Eva / core / IPC client | `cargo test --locked --offline -p jwm-eva -p jwm_eva_core -p jwm_ipc_client`：合计 283 passed |
| 安装脚本 | `bash scripts/test-install-eva.sh` 通过 |
| 原生浮层与渲染 | `eva_` 过滤器：38 passed、1 ignored；被忽略的画廊随后另行显式运行通过 |
| 原生 JWM EVA | `jwm::eva::tests`：55 passed |
| 中文字体纯测试 | `compositor_font::tests`：24 passed；默认字体及中文回退相关纯测试通过 |
| 主题对比 | `every_theme_keeps_osd_and_toast_ink_readable`：1 passed |
| 显式 GLES 画廊 | `eva_overlay_visual_gallery -- --ignored`：1 passed；软件 EGL 上经过实际 GLES 渲染路径，输出 24 张状态图（16 张常规、8 张小屏），覆盖深浅主题、640×360 与 320×180；实际复看中文、双行小屏澄清、确认卡麦克风提示与完整审阅呈现 |
| 格式与差异 | `cargo fmt --all -- --check` 与 `git diff --check` 通过 |
| 未执行项目 | 4 个原生音频隔离图用例及 1 个真实 API 握手用例 ignored；未使用真实麦克风或真实 API 验证本轮 UI 对话体验 |

表中各原生测试过滤器有交集，不将这些计数相加；283 项只对应本轮 Eva/core/IPC 命令，不重复累加前面版本的通过数量。实际 GLES 静态状态截图与界面接线测试不代表语音理解、采集、播放、打断或桌面任务已端到端通过。

在准备好 README 中的原生音频和构建环境后，可重生成界面产物：

```bash
JWM_EVA_GALLERY_DIR=target/eva-ui-preview JWM_REQUIRE_HEADLESS_GL=1 \
  EGL_PLATFORM=surfaceless LIBGL_ALWAYS_SOFTWARE=1 \
  cargo test --locked --offline -p jwm --lib eva_overlay_visual_gallery \
  -- --ignored --test-threads=1
```

`manifest.json` 记录 24 个状态与尺寸，`eva-gallery.png` 是 16 张常规状态的拼板；
`small-glass-dark-clarify.png` 等文件用于核对小屏换行。另有真实 GLES 像素用例输出
`listening-gpu.png` 与 `confirmation-eight-targets.png`。这些预览产物不加入源码仓库，
也不包含真实音频或 API 回答。

## 2026-10-30 现场连续对话复测

本轮整体**未通过**。在用户真实 X11 会话、PulseAudio 麦克风和
`gpt-realtime-2.1-mini` 上逐轮核对用户听感、原生桌面后状态、API 事件和音频流。
成功、失败及重试分别保留在
[现场报告](eva-live-e2e-20261030.json)，不据此计算完整语料成功率或延迟分位数。

| 场景 | 实测结果 |
|---|---|
| 快捷键开麦、切第三个桌面 | 已确认真实输入流、云端就绪、`switch_tag` 成功回执及 tag mask=4；旧配置 `brief` 的简单成功只播提示音 |
| 连续指令修改布局 | 同一会话完成切桌面及网格布局，真实后状态为 `grid`；当时语音回复仍存在提前降级，不能把听到开头算完整播放 |
| 数一到十 | 初始播放缓冲溢出；修复后又受旧的 256 输出上限影响；将上限调至当前默认 1024 后，用户完整听到一到十，原生播放确认 `drained` |
| 撤销 | 一次因手动切换桌面被 `undo_conflict` 拒绝；同桌面复测又将撤销误判为继续不存在的待续任务，没有派发撤销动作，仍未通过 |
| 连续对话收尾 | 完整数数后的旧实现曾报 `audio_sequence_invalid`，浮层消失不算正常收尾通过；补上未归属 commit 的过滤与回归后，终轮可进入后续轮次并最终释放资源，但不扩大为所有结束语料通过 |
| 自然打断 | 用户报告未能打断；终轮先出现 `pulse_playback_underrun`，没有观察到 `speech_interrupted`。此轮未通过，也不足以单独判定起声门限或回声处理的根因 |
| 暂时超时 | 曾在云端配置成功、尚无起声事件时报告超时；后来的短连接检查正常，最初超时源仍未核清 |
| 测试收尾 | 原生状态为 Idle、浮层关闭、唤醒关闭；实际输入与输出流全部归零，当前桌面核对为平铺 |

本轮安装了当前源码，并保留原模型和凭据配置。交互配置改为语音反馈、全双工、
30 秒续聊及 1024 语音输出上限；原配置和程序已有本地备份。代码修复包括：
在两秒播放积压上限内等待容量、三秒停滞超时与取消；已知撤销预检拒绝的对话反馈；
无效只读能力分类的一次有界修正；未归属服务端音频提交不取得执行权、不破坏当前输入；
仅输出固定诊断码。播放断流和语音撤销误判仍保留为缺陷，没有以安装修复替代实测通过。

本地验证：Eva 188 passed / 0 failed / 4 ignored，core 40 passed，undo IPC 2 passed；
另外实际执行私有 PulseAudio 两项以及 PipeWire 24/48 kHz 各两项，全部通过。
私有 PulseAudio 四秒突发输入完整排空约 4034 ms，保留两秒积压界限；这些隔离结果不替代实体语音。
格式与差异检查通过。严格 Clippy 仍被源码原有的 12 条告警阻断；仅允许这三类既有告警后，
全目标检查通过，不能把它写成严格零告警通过。mini 唤醒、Wayland 实体设备、完整语料、
锁屏和拔设备矩阵及 P50/P95 均未在本轮验证。

### 同日断流与撤销修复后复测

后续修复及本机安装信息见
[修复后复测报告](eva-live-e2e-20261030-retest.json)。此前失败记录保留；
当前实体语音结果仍待用户复测，不能以本地通过替代端到端通过。

- 私有 PulseAudio 测试先复现两段音频间隔 650 ms 就报
  `pulse_playback_underrun`。改为核验实际协商的非零预缓冲，允许原生补缓冲；
  结束时显式放行不足预缓冲长度的尾音。保留两秒积压上限、取消和原生播放时钟。
  PulseAudio 的非零预缓冲与断流恢复语义见
  [官方缓冲说明](https://freedesktop.org/software/pulseaudio/doxygen/structpa__buffer__attr.html)。
- 撤销已完成操作明确使用 `task_intent=new`。若模型仍提交其他任务分类，
  不直接更改分类执行，而是使用原始已归属输入进行一次有上限的计划修复。
  禁用修复、轮次耗尽或再次分类错误均不能获得撤销执行权；原有任务结果与目标校验保留。
- Eva、core、IPC client 回归合计 **291 passed / 0 failed / 5 ignored**。
  另显式运行私有 PulseAudio 两项，全部通过且无残留音频流：四秒突发完整排空约
  4036 ms；650 ms 断流后续播及 20 ms 尾音完整排空；补缓冲后取消仍在
  200 ms 断言界限内返回，只截断已播放部分并删除未听到的后续项。
- Eva 包格式及差异检查通过。Clippy 在沿用三类既有告警豁免后通过，
  不声明严格零告警。修复版已安装，配置内容校验保持一致；布局撤销、自然打断和
  先前偶发超时仍以现场观测与用户听感为准。

随后用户反馈“正常”，但补查服务日志发现第一次续聊在发现、规划和修复后仍以
`plan_invalid` 退出；恢复布局由新会话的 `set_layout` 完成。因此不能将该反馈记为
同会话语音撤销已通过。两次实体语音回复均观察到 `drained`，也实际观察到
`unowned_input_ignored` 后可以进入下一轮；失败与成功分别保留在复测报告。

后续修复按本地待续任务状态收窄 `task_intent` 的候选值：仅 ready 可继续，
ready 或目标变化待修正可修改；没有待续任务及结果未知时不提供继续选项，
同时保留本地校验与有上限的修复。新增计划拒绝的固定诊断码，排除模型参数和错误正文。
更新后合计 **292 passed / 0 failed / 6 ignored**，Clippy 沿用原有三类告警豁免后通过。
显式真实模型文字探测只规划、不采音或执行桌面动作，使用实际费用账本：修复前一次需重试；
候选收窄后一次未得到完整计划，最近一次约 5130 ms 正确提交 `undo_last_plan`，无需重试。
这些探测不替代语音验收。当前版本已安装且配置未改，下一轮等待实体插话打断反馈。

实体插话复测失败：用户反馈没有停止数数、也没有关麦；原生输入流在回复时持续存在，
没有 `speech_interrupted`，数数完整排空后进入续聊，随后另报 `realtime_write_timeout`。
资源最终释放，之后的本地取消不计作语音关麦成功。

已复现并修正本地门限的两种漏检：轻声低于固定 0.025 RMS，以及短字间隔消除累计起声。
改为听音时校准底噪、执行和播放时冻结门限，并在十个 20 ms 帧内要求六个非回声语音帧；
保留回声拒绝和全部后续输入归属、意图与执行校验。只记录每轮的数值统计，不记录音频或转写。
当前回归 **296 passed / 0 failed / 6 ignored**；私有 PulseAudio 两项通过，
其中原生采集结合内存中控制至 0.012 RMS 的合成 PCM 实际触发停止原生播放，
停止约 1 ms，历史只保留听到部分。合成源音量校准曾两次失败，均保留在报告；
最终测试使用受控 PCM，避免把两秒突发合成源当成真实设备的实时音量测量。
新版本已安装、配置保持一致；该隔离结果不代表实体插话已通过，正在等待再次实测。

接着用户反馈“麦克风状态异常”。本轮在规划后触发了五秒看门狗并被 systemd 重启，
未进入可验收的打断阶段。只规划、不采音或执行桌面动作的真实模型探测也复现退出：
文件系统提交使一次 `fsync` 阻塞约 3.3 秒，并占住运行时驱动。已将持久化事务移至
阻塞线程，保留独占访问、执行前可靠落盘、取消及结果未知时不重放的约束；取消先释放
音频和原生执行权，再处理费用收尾。迟到的撤销收尾也不能覆盖较新的撤销历史。

修复后同类无麦克风探测约 4240 ms 完成且服务存活；本次磁盘压力较低，不能据此声明
相同慢盘负载下的实测延迟。确定性测试额外验证：一秒看门狗在 1500 ms 慢写期间保持
心跳，实际卡死仍以 70 退出；私有 PulseAudio 在同样慢写期间用受控 0.012 RMS 合成
PCM 打断原生播放，停止约 11 ms，先于落盘完成。两个原生音频测试均通过且无残留流。
最终 Eva 与 IPC 回归为 **257 passed / 0 failed / 6 ignored**。另一次联合运行中 core
为 41 passed / 2 failed，原因是并行改动新增的 `show_desktop` 未进入 Eva 动作清单；
不能声明联合全绿。格式检查与沿用三类既有告警豁免的 Clippy 通过。

安装后又发现独立的浮层就绪问题：实际桌面的 X11 抽象套接字可用，但文件系统 X0
路径缺失；升级后的 x11rb 0.14 普通连接不再回退，导致误报 `overlay_unavailable`。
输出检测已改用共享的、保留认证的同显示器连接，并继续核验 DPMS、窗口映射及显示器
几何。两项隔离浮层测试通过，主程序已安装并保留窗口重启；窗口、各桌面布局和当前
视图核验一致。`doctor` 全部就绪，待机麦克风关闭，v4 服务尚无重启。
已开始新一轮实体插话复测，结果仍待用户听感和现场日志共同确认。

推送前复核另外补齐了启用转写时的唤醒交接落盘路径；当前实体配置关闭转写，
上述已安装版本及测试记录保留原哈希。也将原生手势/CLI 的 `show_desktop` 登记为
内部工作流，并断言它不会成为模型工具；没有新增语音执行权限。这两处后续源码
改动尚未替换桌面上的二进制。
补齐后的联合回归为 **300 passed / 0 failed / 6 ignored**，上述两项能力清单失败已消除；
Clippy 沿用三类既有告警豁免后通过。实体打断仍待反馈。

### master 9c8da887 后的离线事件归属回归

本轮以 `9c8da8875de7d22d585cee395cb1e2c10e4ee9bd` 为基线，复核了上述
撤销分类、任务状态、撤销执行器、Realtime 事件、原生播放队列和打断状态机。
基线已包含撤销分类收窄及有界修复、PulseAudio 非零预缓冲和短尾音放行、
自适应起声检测与慢盘隔离；这些不计作本轮新增实现。C06 的真实暂时超时根因
仍未确认，实体 C10/C12 也没有新增通过证据。

新增修复针对打断后的迟到反馈事件：在新请求收到 `response.created` 前，
`trace.response` 为 None，旧比较式将缺失的事件 ID 与 None 比较，可能放行
旧音频、无归属转写或无 ID 的结束事件。反馈现在必须先有已确认的响应 ID，
再要求事件匹配该 ID；费用回执仍独立处理。没有修改桌面权限、撤销候选状态校验、
执行回执或未知结果不重放规则。

`session::tests::feedback_rejects_unowned_events_before_and_after_response_created`
通过本地 WebSocket 在 `response.created` 前后注入迟到/缺失 ID 的音频、转写和
结束事件，并断言不会创建播放历史、污染回复或派发操作。注入音频使用故意无效的
base64，若被接受即失败，测试无需打开音频设备。恢复旧过滤式运行此测试明确得到
`invalid_feedback_audio`；恢复修复后通过。它验证事件隔离，不代表现场打断成功。

本轮环境为无桌面、无实体音频及无已配置 API 凭据的云端容器，Rust 1.99.1：

| 验证 | 结果及边界 |
|---|---|
| Eva 全量 `--lib` | 227 passed / 4 failed / 5 ignored；四项 lifecycle 测试均在读取子进程 `/proc` start ticks 时得到 ENOENT |
| Eva 排除上述四项环境受限测试 | 227 passed / 0 failed / 5 ignored / 4 filtered out；包括 C10 计划修复、轻声/字间停顿打断、回声拒绝、慢盘心跳与新事件隔离回归 |
| `jwm_eva_core --lib` | 43 passed / 0 failed；包含撤销冲突与未知结果不可重放 |
| `jwm_ipc_client --lib` | 8 passed / 9 failed；九项均在 Unix socket 创建/绑定处得到 EPERM，不能声明 IPC 集成通过 |
| 私有 PulseAudio fixture | 未就绪；`module-native-protocol-unix` 创建 socket 得到 EPERM，未执行原生音频测试；不计为合成音频通过 |
| 真实设备/API | 未执行；没有替代或覆盖原有实体失败记录 |
| 格式与差异 | `cargo fmt -p jwm-eva --check`、`git diff --check` 通过 |
| Clippy 全目标 | 豁免既有 `deprecated`、`collapsible_if`、`needless_borrow`、`large_enum_variant`、`chunks_exact_to_as_chunks` 后通过；未豁免最后一项时原有音频代码出现四处告警，不声明严格零告警 |

复现新回归：`cargo test -p jwm-eva --lib feedback_rejects_unowned_events_before_and_after_response_created`。
具备原生 Unix socket 和音频依赖的环境应继续执行 `eva/tests/native-pulse.sh`，
然后在授权实体设备上复测“修改布局→撤销”及播放期间“别数了，关闭麦克风”，
结合 `speech_interrupted`、响应取消、实际停止听感和资源释放共同验收。
