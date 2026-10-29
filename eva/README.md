# JWM Eva

Eva 是快捷键激活的独立语音服务。默认 Alt+Ctrl+E 开始一段中文会话，再按同一快捷键或 Esc 取消。反馈结束后保留 5 秒继续说话窗口；Idle 释放麦克风。需要确认的操作由本地 Alt+Ctrl+Enter 批准，模型不能代替确认。

## 日常口语与交互

浮层按状态、操作结果或澄清问题、输入字幕和快捷键分层显示，沿用 JWM 的玻璃／磨砂主题。结果会保留到继续说话阶段；倒计时来自这一轮真实截止时间，麦克风电平和服务心跳不会重新开始计时。输入字幕仍需显式开启 `input_transcription`。

可以说“切回刚才那个窗口”“这个窗口先收起来”“宽一点”“声音小一点”或“切到三号桌面，然后用网格排一下”。最近窗口、屏幕左右和当前工作区来自本地桌面状态；有多个合理目标时先问清楚。单次已确认的小幅调整后可继续说“再一点”或“反过来一点”。完整示例、默认步长及验收方式见 [日常口语与交互验收](../docs/eva-language-ux.md)。

音量和本机背光支持绝对百分比及相对调节，“一点”默认 5 个百分点。每项操作等待关联的原生控制回执，实际读回成功后才继续后续步骤；读回不可用时报告未知且不重复发送。背光的硬件刻度可能无法精确达到指定百分比，结果以实际读回为准；这些控制不提供任意外接屏幕亮度调节。

## 安装

先按仓库的 [JWM 构建与安装说明](../README.md#build-and-verify) 准备依赖，再使用 [现有安装器](../scripts/install_jwm_scripts.sh) 安装本次修改后的 JWM、bridge 和 player。仅运行 `cargo build` 不会替换已安装或正在运行的旧版本；旧版本没有执行许可和受条件保护的接口。

```bash
bash scripts/install_jwm_scripts.sh
```

安装完成后注销，在登录界面选择本次安装的 JWM 会话重新登录。已经运行本次构建的用户可跳过这一步。以下命令在新会话的仓库根目录执行。Eva 单独构建，不把云端连接放进窗口管理器：

```bash
bash scripts/install-eva.sh
```

安装器把程序放到 `~/.local/bin/jwm-eva`，配置放到 `~/.config/jwm/eva.toml`，安装绑定图形登录会话的 systemd 用户服务，并启动处于 Idle 的 daemon。已有配置会保留。自定义安装路径可用 `--prefix`；只安装文件可用 `--no-enable`。查看全部选项：

```bash
bash scripts/install-eva.sh --help
```

构建 Eva 音频模块需要 `pkg-config`、`libpulse-dev`、PipeWire/SPA 开发头文件以及 `clang`/`libclang-dev`。Rust 的 libspa 绑定及实际加载的 PipeWire 客户端库需要 ≥ 1.2；安装器会先调用 `scripts/ensure_pipewire.sh --env`，在系统开发包不足时构建私有开发前缀并配置 rpath。这个客户端可连接 ≥ 0.3.44 的 PipeWire 服务端及其会话管理器；旧服务端缺少可靠时间证据时作废或澄清该句，不用事件接收时间猜测执行对象。音频不依赖 `pw-record`/`pw-play` 子进程。音频连接或格式失效会作废整句；播放完成依据原生排空回执和播放时钟。

按快捷键激活时，Eva 自动连接当前用户的原生音频后端：真实 PulseAudio 服务使用原生 PulseAudio 采集和播放；PipeWire 桌面继续使用原生 PipeWire。无需停止 PulseAudio、切换全局音频服务或手动启用设备。会话结束和取消会断开本次音频流，Idle 不保留麦克风流。

`jwm-eva doctor` 显示 `audio_backend`、输入和输出设备是否可用；诊断只读，不打开音频流。PipeWire 后端需要真实 `Audio/Source` 与 `Audio/Sink` 节点，不能用仅有摄像头或屏幕视频的连接代替。PulseAudio 后端只连接当前用户的本机 Unix socket，拒绝把扬声器 monitor 当作麦克风；显式指定的输入设备消失时结束该句，不悄悄切到别的设备。

API 凭据默认从 Secret Service 读取；在桌面会话中运行下面命令，按提示输入 key（不会写入配置文件）：

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

不要同时给这个组合绑定 `spawn ["jwm-eva", "toggle"]`。X11 需要启用 JWM 合成器以显示开麦浮层；浮层不可用时拒绝激活。运行中失去浮层或锁屏会撤销执行许可。首次安装后用 `systemctl --user status jwm-eva.service` 查看服务状态。

确认卡列出本地冻结的每项动作、稳定目标引用和完整目标值。长参数可按 Alt+Ctrl+D 打开详情，用 PageUp/PageDown 查看；全部详情页看过后才能按 Alt+Ctrl+Enter 批准。Esc 随时取消。需要较长审阅时间时，在 `[execution]` 调整 `confirmation_timeout_ms`（1000–600000；默认 15000）；心跳不会延长当前确认期限。

可撤销时，浮层提供 Alt+Ctrl+Z 入口；执行时仍会检查窗口实例和手动修改冲突。错误卡可按 Alt+Ctrl+R 重试，Alt+Ctrl+I 打开本地诊断。默认不显示输入字幕；显式开启 `input_transcription` 后只显示对应已提交音频的实际异步转写，字幕不参与执行授权，也不写入日志或账本。

首版要求保持半双工和本地起声检测开启，关闭这些必要条件会直接报配置错误。本地起声用于跟进窗口的截止判断；执行前还会用实际成功上传的 PCM 区间核对服务端音频 item 的归属。无法证明输入时序或窗口起声上下文时，会停止或要求重新说，不改用新焦点猜测。

浮层可以迁移到仍可显示的输出，操作目标保留原激活显示器的范围。原目标已拔出时需要重新激活；迁移浮层不会把旧命令自动改到另一块屏幕。

## 接入 API 后验证

1. 按 Alt+Ctrl+E，等浮层显示 Listening 和开麦电平。
2. 说“切到第三个 tag”，再跟进说“换成 scrolling 布局”。核对同一显示器和工作区。
3. 说“把这个窗口移到第八个 tag，然后切过去”。核对同一窗口实例。
4. 等待模型时按 Esc，确认迟到计划没有执行；再在等待期间切换应用，确认不会误改新窗口。
5. 会话退出后检查实际 PipeWire/PulseAudio 采集流已释放。不能只根据浮层文字判断。

完整 X11/Wayland、设备故障、费用、语义和延迟验收清单在 [验收账本](../docs/eva-acceptance.md)。本地测试和实时 API 测试分别报告；没有 key 或没有图形会话的检查不算语音端到端通过。

## 诊断与开发

```bash
jwm-eva status --json
jwm-eva capabilities --json
jwm-eva cancel
jwm-eva dry-run --text "切到第三个 tag"
journalctl --user -u jwm-eva.service -f
```

`dry-run` 会显示模型并执行一次有界云端规划与本地校验，始终关闭桌面修改权限，可能产生 API 费用。`capabilities` 同时报告名字登记和用户语义能力，不能把别名数当作独立功能数。设备和媒体的 `accepted` 表示已提交；只有真实后置条件或带关联标识的完成回执才能变成 `succeeded`。超时的 `unknown` 不会自动重发。

`doctor` 同时只读显示本机当日已结算、在途预留和未知费用，按模型与价格版本汇总，并给出官方账户用量/额度入口。它不会调用模型、采集声音或修改账本；本地估算不代表整个账户的总账。

修改配置后重启 `jwm-eva.service`。`toggle`、`dry-run` 和 `undo` 会核对客户端所选配置与运行中 daemon 的配置，避免模型、凭据来源或预算设置不一致；显式指定的 `--config` 文件缺失会报错。取消和状态查询在配置损坏时仍可使用。预算默认采用系统本地时区，示例配置显式选择 `Asia/Shanghai`，也支持 `UTC`。

应用启动仅使用配置 `[applications]` 中登记的 ID → argv，按本机已安装程序填写；模型不能提交 shell 字符串。撤销按最近已验证计划的逆序执行，桌面被手动修改后会拒绝有冲突的补偿。`hi eva`、双工自然打断和 jagent 扩展当前保持关闭。

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

有凭据时可显式执行仅连接和配置的实时契约检查。该测试使用代码中的 `Config::default()`，不读取已安装的 `eva.toml`、Secret Service 或 `api_key_file`，并要求当前测试进程的 `OPENAI_API_KEY`。它不采集声音、不操作桌面，也不替代使用实际安装配置的语音演示。

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
