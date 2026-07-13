# JWM 内置录音组件

JWM 可以直接通过 ALSA 采集默认麦克风并写入标准 PCM WAV 文件。录音过程中不会
启动 `arecord`、ffmpeg 或其他桌面录音软件。

## 使用

- `Alt+Ctrl+M`：开始/停止录音。
- 默认保存到 `$XDG_MUSIC_DIR`，未设置时保存到 `~/Music`。
- 默认文件名为 `jwm-recording-YYYYMMDD-HHMMSS.wav`。

也可以通过 IPC 精确控制：

```bash
jwm-tool msg start_audio_recording --args '{"path":"/tmp/note.wav"}'
jwm-tool msg get_audio_recording_status
jwm-tool msg stop_audio_recording
```

## 配置

以下字段位于配置文件的 `[behavior]`：

```toml
audio_recording_device = "default"       # ALSA capture device
audio_recording_output_dir = "/home/me/Music"
audio_recording_sample_rate = 48000
audio_recording_channels = 1              # 1 或 2
```

采集设备不支持请求的采样率时，ALSA 会选择最接近的值；实际格式可通过
`get_audio_recording_status` 查询。停止 JWM 或紧急退出特殊模式时，组件也会停止
采集并补全 WAV 文件头，避免留下不可播放的文件。
