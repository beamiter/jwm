# JWM Eva

Eva 是 JWM 的独立语音服务。默认 Alt+Ctrl+E 直接启动 `gpt-realtime` 标准版会话，再按同一快捷键或 Esc 取消。也可用可配置快捷键开启 `gpt-realtime-mini` 全时监听，说“hi eva”后切到标准版。默认反馈后保留 30 秒继续说话窗口；未启用唤醒监听时，Idle 释放麦克风。需要确认的操作由本地 Alt+Ctrl+Enter 批准，模型不能代替确认。

## 日常口语与交互

唤醒待机使用紧凑提示，进入对话后展开状态卡：先看当前状态或结果，再看操作内容、麦克风和此刻可用的快捷键。浮层沿用 JWM 的玻璃／磨砂主题，X11 与 Wayland 共用内容层级。完成、部分完成、等待核验和需要补充会分别呈现，结果保留到继续说话阶段；倒计时来自这一轮真实截止时间，麦克风电平和服务心跳不会重新开始计时。输入字幕仍需显式开启 `input_transcription`。

多步操作会显示“已确认完成 1/3 步 · 正在核验第 2 步”等真实进度。完成数来自本地关联回执和后置条件，正在准备、正在执行、等待核验以及结果未知分别显示；已提交的 `accepted` 不计为成功，模型说“完成了”也不会推进计数。`jwm-eva status --json` 的 `data.task_progress` 包含完成数、总步数、当前步骤和阶段，随当前计划及轮次更新。任务完成比例与麦克风电平分开显示：前者依据本地核验完成数，全部成功核验才显示满进度；后者只跟随实际采集信号。静音输入的电平保持安静，不显示模拟波形；采集状态无法证明时明确显示待确认，不把租约过期当作已经释放麦克风。

可以说“切回刚才那个窗口”“这个窗口先收起来”“宽一点”“声音小一点”或“切到三号桌面，然后用网格排一下”。最近窗口、屏幕左右和当前工作区来自本地桌面状态；有多个合理目标时先问清楚。单次已确认的小幅调整后可继续说“再一点”或“反过来一点”。完整示例、默认步长及验收方式见 [日常口语与交互验收](../docs/eva-language-ux.md)。

音量和本机背光支持绝对百分比及相对调节，“一点”默认 5 个百分点。每项操作等待关联的原生控制回执，实际读回成功后才继续后续步骤；读回不可用时报告未知且不重复发送。背光的硬件刻度可能无法精确达到指定百分比，结果以实际读回为准；这些控制不提供任意外接屏幕亮度调节。

默认使用语义断句 `semantic_vad`，说到一半停顿或补充“然后……再……”时，由模型结合语义判断是否结束这一轮。`[turn_detection].eagerness = "auto"` 自动调整等待；`low` 更愿意等用户补充，`high` 更快接话，`medium` 居中。偏好固定静音间隔时可改为 `type = "server_vad"`，此时使用 `threshold`、`prefix_padding_ms` 和 `silence_duration_ms`。唤醒待机始终采用 `server_vad`，不会跟随标准版的语义断句设置。

`[audio].noise_reduction` 默认 `near_field`，适合耳机和近距离麦克风；房间麦克风可试 `far_field`，已有合适音频处理时可设为 `off`。这是云端输入降噪。原生音频后端另向桌面音频服务请求回声消除，并结合本地播放相关性抑制误打断；效果取决于音频服务和设备，扬声器环境需实测，耳机通常更稳定。

## 安装

先按仓库的 [JWM 构建与安装说明](../README.md#build-and-verify) 准备依赖，再使用 [主安装器](../scripts/install-jwm.sh) 同时编译安装 JWM、bridge、player 和 Eva。Eva 沿用主安装器的 debug/release 模式、并行任务数和构建目录；已有语音配置保留。`--skip-eva` 可跳过 Eva，`--skip-jwm` 的仅面板安装也不会更新 Eva。仅运行 `cargo build` 不会替换已安装或正在运行的旧版本。

```bash
bash scripts/install-jwm.sh
```

安装完成后注销，在登录界面选择本次安装的 JWM 会话重新登录。已经运行本次构建的用户可跳过这一步。主安装器会在当前用户服务和图形会话可用时启用并重启 Eva；否则先安装文件，并提示登录后启用服务的命令。Eva 仍是独立进程，不把云端连接放进窗口管理器。

如果只需要更新 Eva，可以单独运行：

```bash
bash scripts/install-eva.sh
```

安装器把程序放到 `~/.local/bin/jwm-eva`，配置放到 `~/.config/jwm/eva.toml`，安装绑定图形登录会话的 systemd 用户服务，并启动处于 Idle 的 daemon。已有配置会保留。自定义安装路径可用 `--prefix`；只安装文件可用 `--no-enable`。查看全部选项：

```bash
bash scripts/install-eva.sh --help
```

构建 Eva 音频模块需要 `pkg-config`、`libpulse-dev`、PipeWire/SPA 开发头文件以及 `clang`/`libclang-dev`。Rust 的 libspa 绑定及实际加载的 PipeWire 客户端库需要 ≥ 1.2；安装器会先调用 `scripts/ensure_pipewire.sh --env`，在系统开发包不足时构建私有开发前缀并配置 rpath。这个客户端可连接 ≥ 0.3.44 的 PipeWire 服务端及其会话管理器；旧服务端缺少可靠时间证据时作废或澄清该句，不用事件接收时间猜测执行对象。音频不依赖 `pw-record`/`pw-play` 子进程。音频连接或格式失效会作废整句；播放完成依据原生排空回执和播放时钟。

按快捷键激活时，Eva 自动连接当前用户的原生音频后端：真实 PulseAudio 服务使用原生 PulseAudio 采集和播放；PipeWire 桌面继续使用原生 PipeWire。无需停止 PulseAudio、切换全局音频服务或手动启用设备。会话结束和取消会断开本次音频流。开启唤醒监听后，正常对话结束会重新进入 mini 待机；关闭唤醒监听后，Idle 不保留麦克风流。

默认开启的 `[openai].keep_warm` 让 daemon 在没有对话、也没有演练时预先保持一条已完成配置的标准版云端会话：按下快捷键后直接复用，省去 TCP、TLS、WebSocket 升级和 `session.update` 往返（约 2 秒），浮层几乎立即进入“正在听”。预热期间不上传任何音频、桌面上下文或工具，也不请求模型回答，因此没有 token 费用；它也不持有执行许可，激活时仍照常向 JWM 取许可和执行上下文。激活前会先做一次 ping 存活探测，过期或断开的连接自动回退到原来的冷连接路径。预热会话每 50 分钟或在服务端到期前 5 分钟轮换，断开后按 5 秒起、最长 5 分钟的退避重连；凭据、模型权限等永久性错误每 10 分钟才重试一次。`jwm-eva status --json` 的 `warm` 字段显示 `disabled`、`connecting`、`ready`（含 `age_ms`）或 `waiting`（含 `error_code` 与 `retry_in_ms`）；日志中的 `warm_ready`、`warm_taken`、`warm_lost`、`warm_rotated`、`warm_connect_failed`、`warm_stale` 只含固定类别码，`cloud_ready.result = "warm"` 表示该次会话复用了预热连接。不希望空闲时保持云端连接时设置 `keep_warm = false`。

`jwm-eva doctor` 默认检查所选配置、凭据是否可读取、音频设备和 JWM 协议，并显示本地费用；只查询设备，不打开音频流或云端会话。报告包括 `audio_backend`、输入和输出设备、模型、断句与降噪设置，并按具体故障给出修复建议。PipeWire 后端需要真实 `Audio/Source` 与 `Audio/Sink` 节点，不能用仅有摄像头或屏幕视频的连接代替。PulseAudio 后端只连接当前用户的本机 Unix socket，拒绝把扬声器 monitor 当作麦克风；显式指定的输入设备消失时结束该句，不悄悄切到别的设备。

首次配置或更换 API key，在终端运行：

```bash
jwm-eva config api-key
```

命令会提示选择保存方式并隐藏密钥输入，自动更新 `eva.toml` 的凭据设置，保留其他设置和注释。默认使用当前配置旁的 `openai.key`（若已指定绝对 `api_key_file` 则继续使用），文件权限为 `0600`；也可选择系统钥匙串。密钥不写入 TOML、不作为命令参数传递，也不会回显。空输入、Ctrl-C 或 Ctrl-D 取消，不保存密钥、不重启服务。保存成功后自动执行 `systemctl --user restart jwm-eva.service`，再检查服务是否运行；只有两步均成功才显示重启成功。重启失败会返回非零状态，但保留已保存的密钥和配置，并提示重试方法，无需重新输入密钥。不要同时用其他编辑器修改配置；检测到并发变化会停止保存。

可指定保存方式，或用全局 `--config` 选择另一份配置：

```bash
jwm-eva config api-key --storage file
jwm-eva config api-key --storage secret-service
jwm-eva --config /absolute/path/eva.toml config api-key --no-restart
```

只保存、不重启服务时使用 `--no-restart`，例如配置一份尚未被服务使用的自定义文件。默认重启的是 `jwm-eva.service`，自定义配置须确保该服务也使用同一路径。保存操作不调用 API；自动重启后的 daemon 会按其配置进行连接预热或唤醒待机。

命令完成后可进一步验证 API 连接：

```bash
jwm-eva doctor --live
```

`doctor --live` 只检查云端连接与配置回执，不采音、不提交推理。自定义配置应使用相同的 `--config` 路径运行检查，并确保服务也使用该配置。

系统钥匙串需要 `secret-tool` 和可用、已解锁的 Secret Service；Debian/Ubuntu/Mint 可用 `sudo apt install libsecret-tools gnome-keyring` 安装。也可手动保存密钥（默认凭据来源为 `secret_service`）：

```bash
secret-tool store --label='JWM Eva OpenAI API' service jwm-eva provider openai
jwm-eva doctor
```

也可以在配置 `[openai]` 中选 `api_key_source = "file"` 并指定绝对 `api_key_file` 路径。该文件必须属于当前用户且权限为 `0600`。使用 `api_key_source = "env"` 时，把 `OPENAI_API_KEY` 导入用户服务环境后重启服务。配置和示例见 [dist/eva.toml](dist/eva.toml)。示例费率只用于本地估算，应随实际模型和官方费率更新；日预算不是组织账户的硬限额。

默认绑定会避开已有按键和 chord；如果需要显式配置，在 JWM 配置中使用原生入口：

```toml
[[keybindings.keys]]
modifier = ["Mod1", "Control"]
key = "e"
function = "eva_toggle"
```

唤醒监听使用独立的原生入口，可选择空闲组合键，例如：

```toml
[[keybindings.keys]]
modifier = ["Mod1", "Control", "Shift"]
key = "e"
function = "eva_wake_toggle"
```

该组合切换“监听 hi eva”，`eva_toggle` 则直接开始或取消标准版会话。也可在终端运行 `jwm-eva wake enable`、`jwm-eva wake disable` 或 `jwm-eva wake toggle`，用 `jwm-eva status --json` 查看状态。锁屏会关闭唤醒监听，解锁后需重新开启。

不要同时给直接激活组合绑定 `spawn ["jwm-eva", "toggle"]`。X11 需要启用 JWM 合成器以显示开麦浮层；浮层不可用时拒绝激活。运行中失去浮层或锁屏会撤销执行许可。首次安装后用 `systemctl --user status jwm-eva.service` 查看服务状态。

确认卡列出本地冻结的每项动作、稳定目标引用和完整目标值。长参数可按 Alt+Ctrl+D 打开详情，用 PageUp/PageDown 查看；全部详情页看过后才能按 Alt+Ctrl+Enter 批准。Esc 随时取消。需要较长审阅时间时，在 `[execution]` 调整 `confirmation_timeout_ms`（1000–600000；默认 15000）；心跳不会延长当前确认期限。

可撤销时，浮层提供 Alt+Ctrl+Z 入口；执行时仍会检查窗口实例和手动修改冲突。错误卡按当前故障显示可用操作：可重试时提供 Alt+Ctrl+R，不可重试时保留处理建议和 Alt+Ctrl+I 本地诊断，禁用相应重试入口。普通会话错误卡保留 6 秒供阅读；重试或新会话使用新的提示期限。凭据、模型权限、配置和预算问题需先修复；结果未知的已提交操作不会因重试提示而自动重放。默认不显示输入字幕；显式开启 `input_transcription` 后只显示对应已提交音频的实际异步转写，字幕不参与执行授权，也不写入日志或账本。

本地起声检测必须开启，用于跟进窗口的截止判断；执行前还会用实际成功上传的 PCM 区间核对服务端音频 item 的归属。无法证明输入时序或窗口起声上下文时，会停止或要求重新说，不改用新焦点猜测。连续对话与打断使用 `half_duplex = false`；配置 `feedback = "voice"` 时，成功、澄清和普通聊天均使用标准版语音反馈。执行结果的口头反馈限定为一两句话，并以 `[openai].feedback_result_max_output_tokens`（默认 256）为上限，说完即进入继续说话窗口；澄清只说出那一个问题；闲聊、数数和知识问答仍使用完整的 `feedback_max_output_tokens`，不会被截断。

本地打断检测按监听阶段测得的背景电平校准灵敏度，支持低输入音量；仍要求持续语音并排除已识别的播放回声。诊断日志 `barge_in_progress` 与 `barge_in_window` 仅含累计帧数、电平峰值、检测门限和回声计数，不保存录音或转写。

在理解、确认、执行或回复期间开口，可中止尚未派发的步骤和当前播放，直接接着说修正内容。Eva 保留打断起声的音频，取消旧响应并清理输入后交给新一轮；已经提交的操作仍以真实回执为准，打断不会撤销已经完成的步骤，也不会自动重发结果未知的步骤。回复历史按实际听到的音频截断。用 `jwm-eva cancel` 同时关闭对话和唤醒待机；活动会话中的 Esc 只取消当前对话，已开启的唤醒监听随后恢复。

中断多步任务后，可以说“继续剩下的”“后面的改成第四个桌面”或“只移动，别切过去”。Eva 在本次会话内保留最近一件待续任务，按本地执行回执排除已完成步骤，再用当前目标引用重新规划；修改后的计划重新经过校验和必要的本地确认。结果未知、仍在执行或执行失败的步骤会阻止自动续做，需要先核清结果；目标消失或身份变化时，普通续做会要求澄清；用户明确改为另一个有效目标时，可重新规划修改后的步骤。旧执行许可和批准不会沿用。

“停一下／等等”保留待续任务，期间闲聊也不会清除；“取消这件事／不用做了”清除任务，新的独立操作替换原任务。取消任务不会解除结果未知操作的防重发限制，须等本地账本核清。新请求在意图或计划确定前再次被打断时，旧待续任务会失效，需重新描述。待续任务只保存在内存，最多 32 步、64 KiB，不跨会话或服务重启恢复。超过内存上限的有效指令仍可执行，但不保留不完整任务供续做。

在标准版对话中说“结束这次对话”“先这样”或“再见”，会结束当前会话并清除本次待续任务；此前开启了唤醒监听时，随后回到 mini 待机。说“停止监听”“关闭麦克风”或“别再听了”，则同时关闭当前会话和唤醒待机，之后需用快捷键或 `jwm-eva wake enable` 显式重新开启。“取消这个任务”只取消任务，仍可继续交谈。mini 待机只判断唤醒词，需先唤醒后才能用这些语音结束指令；待机时可用唤醒开关键关闭监听，`jwm-eva cancel` 可直接关闭全部语音监听。

结束指令先释放本地采集、播放和音频队列，再提交本机会话结束请求，以本地状态显示结果。迟到的模型响应不能关闭后来新开的会话；在这期间手动重新设置的唤醒开关也会保留，浮层会提示按新设置结束。`dry-run` 只显示规划中的会话意图，不应用关麦或会话结束。

浮层可以迁移到仍可显示的输出，操作目标保留原激活显示器的范围。原目标已拔出时需要重新激活；迁移浮层不会把旧命令自动改到另一块屏幕。

## mini 待机与标准版对话

示例配置默认不在服务启动时开麦。按唤醒快捷键或运行 `jwm-eva wake enable` 后，麦克风音频会发送给 `[wake_word].model` 指定的 mini 模型判断唤醒词，待机界面显示监听状态。mini 只判断唤醒，不接收桌面动作工具。可以连着说“hi eva，把这个窗口放到第三个桌面，再切过去”，也可以只说“hi eva”后等待回应再说指令。

同句交接支持最长 15 秒的原始唤醒语音，由标准版重新理解，采用原始起声时的窗口和显示器上下文；mini 生成的文字不会成为执行依据。切换时关闭 mini 云端连接，把本地采集流交给标准版，避免重新开麦丢掉后续起声。交接音频只在有界内存中暂存，使用完或取消后清理。音频缺失、时钟不可信或原目标无法核验时不执行原句，改为唤醒后提示重新说完整指令；JWM 实例、唤醒开关或会话代次变化时丢弃旧交接。超过 15 秒的唤醒句会提示分开说，本次不执行，也不截取后半句执行。会话连接失败后可重新唤醒，或直接按激活快捷键再说长指令。

`[openai].model` 指定唤醒后的标准版，`[wake_word].model` 指定待机 mini，默认分别为 `gpt-realtime` 和 `gpt-realtime-mini`。两者使用同一凭据，分别使用 `[pricing]` 与 `[wake_pricing]` 估算费用，共享日预算。云端全时监听按上传的音频时长计费，持续上传静音会很快耗尽日预算；`save_raw_audio = false` 只表示本地不保存录音。默认开启的 `[wake_word].local_speech_gate` 在待机时先做本地语音门控：静音和背景噪声不离开本机，只有持续的类语音段才连同约 0.8 秒的前置音频一起上传给 mini 判断唤醒词，说完后再多上传 1 秒静音让服务端收尾；服务端已经打开的语音段会一直上传到结束，门控不会截断服务端正在判断的句子。门控只决定“上传与否”，唤醒仍只由 mini 的检测结果决定，不改变任何执行授权。`jwm-eva status --json` 的 `wake.upload_gate`、`wake.uploaded_ms` 和 `wake.suppressed_ms` 显示门控位置与累计时长，待机结束时日志记录一条只含帧计数的 `standby_gate_summary`。音乐、电视或持续交谈的环境仍会被上传；需要连续上传时设置 `local_speech_gate = false`。用 `jwm-eva wake disable` 关闭监听并释放麦克风。临时网络或设备故障会关闭采集并重连；等待期间不上传音频。

暂时故障的重连以 `[wake_word].reconnect_ms` 为初始间隔，指数增加并加入 ±20% 抖动，最长等待 60 秒；限流至少冷却 30 秒。连续失败达到 8 次后停止，最多自动重试 7 次。稳定监听 5 秒、成功唤醒、正常轮换连接或用户重新开启会重置失败计数；状态查询短暂失败不会清空已安排的等待。凭据、模型权限、配置、日预算或账户额度错误会直接关闭唤醒请求，修正后需用快捷键或 `jwm-eva wake enable` 重新开启。`status --json` 的 `wake.retry_state`、`retry_attempt`、`retry_limit`、`retry_in_ms` 和 `error_code` 可用于区分等待重试与已停止；浮层显示重连倒计时。

安装器保留已有 `eva.toml`，已有 mini 模型配置不会自动升级成标准版。可先运行 `jwm-eva config migrate --check` 检查；省略 `--check` 也只检查。只有明确识别为旧版内置配置组合时，`--write` 才迁移模型、对应费率与交互默认值，先创建权限为 `0600` 的原始内容备份，再原子替换配置。凭据来源及路径、应用登记、隐私和预算设置保留；自定义模型、费率或交互设置需要对照 [示例配置](dist/eva.toml) 手动审阅，工具不会猜测覆盖。报告只列涉及的字段名、文件与备份路径，不读取凭据或调用 API。

写入前请关闭编辑器和其他配置写入程序。报告中的 `concurrent_writes_supported = false` 表示不支持并发写入；工具会检查已检测到的变化并保留备份，但不提供与其他编辑器之间的文件事务。

迁移命令支持全局 `--config` 选择文件；写入或手动修改后，重启服务使配置生效：

```bash
jwm-eva --config /absolute/path/eva.toml config migrate --check
jwm-eva --config /absolute/path/eva.toml config migrate --write
systemctl --user restart jwm-eva.service
```

报告的 `current` 表示无需迁移，`ready` 表示可迁移，`migrated` 表示已备份并写入，`manual_review` 表示需手动处理。`[future_wake_word]` 作为旧配置段名仍兼容，新配置使用 `[wake_word]`，两者不要同时填写。设置 `enabled = true` 可在服务启动时进入待机。

长指令默认最多 120 秒、每轮最多 32 个动作、最多 8 轮能力发现/规划；超过能力范围或目标不明确时，Eva 会澄清。每轮上下文会列出已预载的 `loaded_actions`（切换工作区、布局、移动和聚焦窗口、显示器、最小化/最大化/浮动/吸附、宽高微调、启动应用、撤销、音量和亮度），这些指令通常一次模型响应即可完成规划，不再先做能力发现；媒体、截图录制、设备、桌面界面和配置类动作仍按需发现。模型把顶层的 `session_intent`/`task_intent` 原样复制进某个动作参数时，本地会在校验前去掉这份相同的副本，避免为此多花一轮修复；值不同的副本仍按未知参数拒绝。模型偶尔会附带名为 `noop`、没有参数的占位动作，这类不存在的动作会被直接去掉；若 execute 计划因此没有任何动作，仍按原规则拒绝并修复，不会被当作“什么都不做的执行”。上下文还包含 `local_time`（本机当前时间，精确到分钟），“现在几点、今天星期几”可直接作为对话回答，不需要能力发现。窗口标题可以用于语义匹配，最终执行仍绑定实际窗口和显示器引用。所有操作都受当前 JWM 的能力目录和本地执行校验约束；模型不能绕过确认、调用任意 shell 或使用不存在的功能。

## 接入 API 后验证

1. 按 Alt+Ctrl+E，等浮层显示 Listening 和开麦电平。
2. 说“切到第三个 tag”，再跟进说“换成 scrolling 布局”。核对同一显示器和工作区。
3. 说“把这个窗口移到第八个 tag，然后切过去”。核对同一窗口实例。
4. 等待模型时按 Esc，确认迟到计划没有执行；再在等待期间切换应用，确认不会误改新窗口。
5. 开启唤醒监听，分别测试“hi eva”后等待回应，以及“hi eva，把这个窗口放到第三个桌面，再切过去”同句指令；核对目标来自起声时窗口，待机费用归 mini、对话费用归标准版。正常对话结束后应回 mini 待机。
6. 在理解、执行或播放回复时说“停一下”，再说“继续剩下的”或修正指令；核对旧计划没有继续派发，已完成动作不被重复或隐式回滚，未知动作不被重发，迟到回复不会继续播放。
7. 测试否定、模糊目标、长连续指令及背景播放，分别记录成功率、误唤醒和误打断。扬声器环境需实测回声影响。
8. 关闭唤醒监听后检查实际 PipeWire/PulseAudio 采集流已释放；锁屏后检查待机麦克风也已释放。不能只根据浮层文字判断。

完整 X11/Wayland、设备故障、费用、语义和延迟验收清单在 [验收账本](../docs/eva-acceptance.md)。本地测试和实时 API 测试分别报告；没有 key 或没有图形会话的检查不算语音端到端通过。

## 诊断与开发

```bash
jwm-eva status --json
jwm-eva capabilities --json
jwm-eva cancel
jwm-eva dry-run --text "切到第三个 tag"
journalctl --user -u jwm-eva.service -f
```

`dry-run` 会显示模型并执行一次有界云端规划与本地校验，始终关闭桌面修改权限，可能产生 API 费用。它的输出另含 `planning`：`responses` 是本轮实际消耗的模型响应数，`elapsed_ms` 是规划耗时，`rounds` 逐条列出每个响应的阶段（plan、discovery、schema_repair）、结果、本地校验错误以及被拒绝的参数原文（最多 4 KiB）。这些细节只返回给本地调用者；正常语音会话不收集模型原文，日志和账本中仍只有固定类别的错误码。

`jwm-eva report` 从当前用户的 journal（默认 `--since today`，也可用 `--input FILE` 读取保存的日志行）离线统计 Eva 自己的遥测记录：会话数与激活到就绪耗时（按冷连接/预热分开）、每轮模型响应数分布、发现与修复轮数、句末到规划完成/核验完成/首段语音/反馈结束的 P50/P95/最大值、回复播放时长、核验结果计数、按阶段的费用与 token、待机门控上传/抑制时长，以及预热连接事件。`--json` 输出完整数据。记录中本来就没有音频、字幕或模型原文，报告也不会向任何地方发送。

```bash
jwm-eva report
jwm-eva report --since -2h --json
```

`capabilities` 同时报告名字登记和用户语义能力，不能把别名数当作独立功能数。设备和媒体的 `accepted` 表示已提交；只有真实后置条件或带关联标识的完成回执才能变成 `succeeded`。超时的 `unknown` 不会自动重发。

`doctor` 默认只读显示本机当日已结算、在途预留和未知费用，按模型与价格版本汇总，并给出官方账户用量/额度入口。本地估算不代表整个账户的总账。用 `jwm-eva --config /absolute/path/eva.toml doctor --live` 可显式检查该配置及凭据来源对应的对话模型和 mini 模型的云端连接与配置回执；只建立会话，不采集或上传声音、不请求模型回答、不操作桌面。普通 doctor 中的“凭据可读取”不等于云端认证或模型权限已通过；live 握手通过也不等于语音理解、播放、打断或功能执行已通过。

JWM 在命令 schema、执行上下文和助手状态中提供附加的 `assistant_protocol` 版本信息：当前最低 `revision = 1`；标准版会话要求受保护执行、语音会话结束和已核验任务进度特性，mini 唤醒另外要求来源身份与起声显示器历史特性。已有桌面执行 `protocol_version = 1` 保持不变；只有旧字段存在仍不足以证明新版兼容。doctor 遇到缺少版本或特性时会提示更新 JWM，并安装 Eva 后重新登录或重启对应服务。`standby_allowed` 单独表示当前锁屏、输出或会话状态是否允许待机，不代表协议版本。`standby_blocked_reason` 会区分 `overlay_unavailable`、`session_locked` 和 `session_exiting`；已支持协议但浮层不可用或正在锁屏时，应恢复相应运行条件，不能把它当作版本不兼容。

修改配置后重启 `jwm-eva.service`。`toggle`、`wake`、`dry-run` 和 `undo` 会核对客户端所选配置与运行中 daemon 的配置，避免模型、凭据来源或预算设置不一致；显式指定的 `--config` 文件缺失会报错。取消和状态查询在配置损坏时仍可使用。预算默认采用系统本地时区，示例配置显式选择 `Asia/Shanghai`，也支持 `UTC`。

应用启动仅使用配置 `[applications]` 中登记的 ID → argv，按本机已安装程序填写；模型不能提交 shell 字符串。撤销按最近已验证计划的逆序执行，桌面被手动修改后会拒绝有冲突的补偿。jagent 扩展仍未接入。

媒体核验需要安装同次构建的 `jwm-bridge` 和 `jwm-player`，并运行 bridge 的 MPRIS 监听。打开播放器只返回已提交；指定文件被播放器报告为正在播放后才确认播放，加入队列则核对实际队列条目增加。缺少读回时保留结果未知，用户可检查播放器后再决定下一步。

开发检查需要准备当前 Bash 的 PipeWire 构建环境。安装器在子进程中导入的变量不会留在当前终端；尤其在使用私有开发前缀时，不能省略这一步。只接受辅助脚本输出的两个明确变量名，不使用 `eval`：

```bash
eva_import_pipewire_env() {
  local eva_environment eva_setting
  eva_environment=$(bash scripts/ensure_pipewire.sh --env) || return
  while IFS= read -r eva_setting; do
    case "$eva_setting" in
      PKG_CONFIG_PATH=*|JWM_PIPEWIRE_PREFIX=*) export "$eva_setting" ;;
      '') ;;
      *) printf '%s\n' 'Unexpected PipeWire environment key' >&2; return 1 ;;
    esac
  done <<< "$eva_environment"
}
eva_import_pipewire_env
```

环境准备成功后，在同一个 Bash 中运行离线检查（Cargo 依赖须已缓存）：

```bash
cargo test --locked --offline -p jwm_eva_core -p jwm-eva -p jwm_ipc_client
cargo test --locked --offline -p jwm --lib jwm::eva::tests -- --test-threads=1
bash scripts/test-install-eva.sh
```

交互式凭据配置的终端测试使用临时目录和占位密钥，覆盖隐藏输入、取消、密钥轮换、文件权限和钥匙串失败，不连接真实服务：

```bash
cargo build --locked --offline --release -p jwm-eva
python3 eva/tests/api-key-pty.py target/release/jwm-eva
```

真实模型语料入口默认忽略，需要明确启用；先停止 Eva daemon 以释放真实费用账本的独占锁。入口在独立子进程中把 `JWM_SOCKET` 固定为私有只读测试服务，桌面发现只读取虚构场景，任何命令都会拒绝。它复用生产规划、参数校验与真实费用预留/结算，不采集声音、不执行动作，也不验证真实桌面后置状态。

```bash
install -d -m 0700 /tmp/eva-corpus-results
JWM_EVA_RUN_LIVE_CORPUS=1 \
JWM_EVA_CORPUS_OUTPUT=/tmp/eva-corpus-results/result.json \
JWM_EVA_CORPUS_CASES=eva-001,eva-090 \
cargo test --locked --offline -p jwm-eva \
  session::corpus_runner::live_model_acceptance_corpus \
  -- --ignored --exact --nocapture
```

省略 `JWM_EVA_CORPUS_CASES` 可遍历全语料；`JWM_EVA_CORPUS_LIMIT` 可限制本次新增记录数。重复使用同一输出路径会跳过已有案例，只有语料、配置、规划代码和 fixture 指纹完全一致才接受断点。每项先持久化 `in_progress`，再记录结果；中断、错误、待人工复核和 fixture 缺失均不自动重跑或算通过，需要复跑时另开结果文件。费用以本地配置费率估算保存，未知预留与已结算分开统计，不等于账户账单。中断项的费用仍在真实账本中，但报告无法完整归属到案例：会标记 `cost_complete=false`、列出 `unfinished_cost_case_ids`，将已归属金额明确显示为下界、总金额留空；恢复也不会把缺失金额默认为零。

当前静态 fixture 可覆盖 225 项，其余 16 项依赖多轮已核验执行历史或窗口实例销毁时序，记录为 `blocked_fixture`，仍保留在 241 项总数中。精确动作和 noop 可自动判分；澄清措辞及只有意图描述的预期记录为 `manual_review`。每项明确标记 `grade_scope=plan_structure_only`，并保留尚未核验的 `unevaluated_invariants`；计划结构通过不代表 UI、回执、防重放或桌面后置条件也通过。`session_intent` 只记录、不参与当前评分，因此也不代表会话结束或关麦语义通过；模型收到的本地时间仍取每项执行时的真实时钟。输出不会计算完整语义或语音成功率；`tests/corpus.json` 的状态也不会被改成已通过。追加的离线测试仅验证隔离、断点和评分边界，不代表实际模型测过。

有凭据时可显式执行仅连接和配置的实时契约检查。开发测试与 `doctor --live` 复用握手逻辑，分别检查对话与唤醒模型；开发测试默认读取正常配置路径（文件不存在时使用内置默认值），也可通过 `JWM_EVA_LIVE_CONFIG=/absolute/path/eva.toml` 选择文件；测试凭据始终由当前进程的 `OPENAI_API_KEY` 提供。日常检查已安装配置及 Secret Service、文件或环境凭据来源时，使用 `doctor --live`。两者都不采集声音、不上传音频、不请求模型回答、不操作桌面，也不替代使用实际安装配置的语音演示。

先完成上面的构建环境准备，再在同一 Bash 中使用临时子 shell 输入密钥。密钥不会进入命令历史或命令参数，子 shell 结束后不保留此环境变量：

```bash
(
  set +x
  IFS= read -rs -p 'OpenAI API key: ' OPENAI_API_KEY || exit 1
  printf '\n'
  export OPENAI_API_KEY
  JWM_EVA_RUN_LIVE=1 cargo test --locked --offline -p jwm-eva --test live_api \
    -- --ignored --exact realtime_configuration_handshake
)
```
