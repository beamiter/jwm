# JWM 内置录音组件

JWM 可以通过 ALSA 采集默认麦克风。PCM WAV 默认使用内置直录后端，不启动外部
录音程序；FLAC、Opus、MP3 等格式由 ffmpeg 后端编码。

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
audio_recording_backend = "auto"         # auto / direct / ffmpeg
audio_recording_format = "wav"           # wav / flac / opus / mp3
audio_recording_bitrate = "128k"         # Opus/MP3 使用
audio_recording_output_dir = "/home/me/Music"
audio_recording_sample_rate = 48000
audio_recording_channels = 1              # 1 或 2
```

采集设备不支持请求的采样率时，ALSA 会选择最接近的值；实际格式可通过
`get_audio_recording_status` 查询。停止 JWM 或紧急退出特殊模式时，组件也会停止
采集并补全 WAV 文件头，避免留下不可播放的文件。

`auto` 会让 WAV 使用内置 ALSA 直录，让 FLAC、Opus、MP3 使用 ffmpeg。`direct`
目前只支持 WAV；选择其他格式时会明确报错。显式 IPC 路径的扩展名决定实际格式，
例如 `start_audio_recording --args '{"path":"/tmp/note.opus"}'`。

## 录屏时同步录制语音

录屏默认会同时从麦克风采集语音，并直接写入 MP4 的 AAC 音轨。视频帧和音频使用
各自的系统时钟时间戳，ffmpeg 的异步重采样会校正长期录制中的轻微时钟漂移。停止
录屏时两个轨道同步结束，分段录制和最终拼接也会保留音轨。

录制开始后，编码器会直接写入 `Videos/recording-YYYYMMDD-HHMMSS.mp4`（或
`recording_output_dir` 指定的目录），不会先在 `/tmp` 创建中间 MP4。目标目录无法
解析或创建时会明确报错，也不会静默退回临时目录。

```toml
# 位于 [behavior]
recording_audio_enabled = true
recording_audio_device = "default"
recording_audio_bitrate = "128k"
```

如果 ALSA 设备不可用，或者当前 ffmpeg 没有 ALSA 输入支持，JWM 会在日志中给出
警告并自动退回纯视频录制。录屏占用麦克风期间不能同时启动独立音频录制；若独立
录音已经运行，开始录屏时会先安全停止并保存当前音频文件。
