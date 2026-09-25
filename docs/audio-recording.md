# JWM built-in audio recorder

JWM can capture the default microphone through ALSA. PCM WAV uses the built-in
direct-capture backend by default and starts no external recorder; FLAC, Opus,
MP3 and the other formats are encoded by the ffmpeg backend.

## Usage

- `Alt+Ctrl+M`: start/stop recording.
- Recordings are saved to `$XDG_MUSIC_DIR`, or `~/Music` when that is unset.
- The default file name is `jwm-recording-YYYYMMDD-HHMMSS.wav`.

IPC gives precise control too:

```bash
jwm-tool msg start_audio_recording --args '{"path":"/tmp/note.wav"}'
jwm-tool msg get_audio_recording_status
jwm-tool msg stop_audio_recording
```

Feedback matches the screen recorder: once the recorder has actually started,
a start card appears carrying the output path, and a normal stop shows a stop
card with the path as well. A failure to start, a failure to stop, or an
unsupported format is shown at urgency 2, which passes Do Not Disturb —
believing the microphone is recording when it is not is a privacy matter and
must not be silent.

While the recorder runs, a MIC chip stays on screen: a red dot plus a static
"MIC" label, the same flat pill as the screen recorder's REC chip (`ui.osd`
background, no glass blur). The standalone recorder lives on the WM side, and
the compositor cannot derive its state from its own capture pipeline the way it
derives REC, so the WM pushes it through the backend trait
(`compositor_set_mic_indicator`). The label carries no timer and is rasterized
once per state change, so it needs no frame pump. The chip takes REC's place
(bottom right, 16 px from the edges); when a screen recording and the
standalone recorder run at once it stacks directly above REC, the two never
overlap, and on a screen too short for both it pins to the top rather than
leaving the screen. It is drawn under the same discipline as REC: X11 draws it
after the frame is captured, and Wayland draws it in the post-delivery chrome
block behind the `tail_domain` gate, so the chip appears only on the local
screen and never leaks into screenshots or encoded video. While it is shown it
blocks direct scanout (a documented side effect: with the chip visible, a
standalone recording keeps the exact-sRGB fallback path). The chip appears
only once the recorder has actually started — the same gate as the start card
— and it is cleared on stop whether or not the file was finalized, and when
the session is torn down. A screen recording shows only the REC chip even when
it captures the microphone: MIC is the standalone recorder's own cue.

### Stopping

The key toggle does not wait for the file. The microphone session ends at
once — the MIC chip clears and the recorder reads inactive — and the stop card
(or the failure toast) follows once the recorder has finished writing the
file. IPC `stop_audio_recording` does wait, so its reply confirms that the file
is finalized.

While a recording the key stopped is still writing its file,
`get_audio_recording_status` reports `"active": false` with
`"finalizing": true`: the microphone is released, but the file is not complete
yet. `finalizing` is `false` at every other time. A new recording cannot start
over it: the key toggle and IPC `start_audio_recording` are refused with
`the previous audio recording is still being finalized; try again in a moment`
(the urgency-2 "Audio recording unavailable" toast, and for IPC an error reply
and an `audio_recording/error` event). An IPC `stop_audio_recording` in that
moment waits for the file and reports the recording as its own
(`"was_active": true`).

### When the recorder stops on its own

A recorder that stops without being asked — the USB microphone unplugged,
ffmpeg killed — is never mistaken for an idle one, and never ends silently.
JWM's periodic tick notices it moments later and ends the session: the MIC
chip clears, and the failure is reported with the recorder's own error
through the urgency-2 "Audio recording failed" toast and an
`audio_recording/error` event. Until then `get_audio_recording_status` keeps
reporting `"active": true` with `last_error` set, and a stop, a toggle or IPC
`stop_audio_recording` in that moment ends the session the same way — the
toggle never quietly reopens the microphone behind a failure nobody saw. An
explicit IPC `start_audio_recording` is still accepted and starts a fresh
recording.

### A capture device that hangs

A recorder that has not initialized within 3 seconds — a capture device whose
open never returns — is abandoned rather than waited on. The start fails with
`audio recorder did not initialize within 3s` instead of hanging the
compositor, and further starts are refused ("the previous audio recorder is
still stuck opening the capture device") until the stuck open returns. When it
finally does, the abandoned recorder releases the device at once and deletes
the empty file it may have left instead of recording.

## Configuration

These keys live in the `[behavior]` table of the configuration file:

```toml
audio_recording_device = "default"       # ALSA capture device
audio_recording_backend = "auto"         # auto / direct / ffmpeg
audio_recording_format = "wav"           # wav / flac / opus / mp3
audio_recording_bitrate = "128k"         # used by Opus/MP3
audio_recording_output_dir = "/home/me/Music"
audio_recording_sample_rate = 48000
audio_recording_channels = 1              # 1 or 2
```

When the capture device does not support the requested sample rate, ALSA picks
the closest one; `get_audio_recording_status` reports the actual format. When
JWM stops, or a special mode is torn down in an emergency, the recorder also
stops capturing and completes the WAV header, so it never leaves an unplayable
file behind.

`auto` uses the built-in ALSA direct capture for WAV and ffmpeg for FLAC, Opus
and MP3. `direct` supports only WAV for now; any other format is an explicit
error. The extension of an explicit IPC path decides the actual format, for
example `start_audio_recording --args '{"path":"/tmp/note.opus"}'`.

## Recording the microphone with the screen

By default a screen recording also captures the microphone and writes it
straight into the MP4 as an AAC track. Video frames and audio carry their own
system-clock timestamps, and ffmpeg's asynchronous resampling corrects the
slight clock drift of long recordings. Stopping the screen recording ends both
tracks together, and segmented recordings keep the audio track through the
final concatenation.

Once recording starts, the encoder writes directly to
`Videos/recording-YYYYMMDD-HHMMSS.mp4` (or the directory `recording_output_dir`
names) without an intermediate MP4 in `/tmp`. When the target directory cannot
be resolved or created, that is an explicit error; it never silently falls
back to a temporary directory.

```toml
# in [behavior]
recording_audio_enabled = true
recording_audio_device = "default"
recording_audio_bitrate = "128k"
```

If the ALSA device is unavailable, or the installed ffmpeg lacks ALSA input
support, JWM logs a warning and falls back to a video-only recording. While a
screen recording holds the microphone, the standalone recorder cannot start;
if the standalone recorder is already running, starting a screen recording
first stops it safely and saves the current audio file.

## Dynamic recording region

- `Alt+Ctrl+R`: select a region and press Enter to start recording; pressing it
  again while recording stops.
- `Alt+Ctrl+Shift+R`: enter region adjustment mode while recording.
- In adjustment mode, drag inside the region to move it, or drag an edge or a
  handle to resize it; Enter confirms, and Escape restores the region from
  before the adjustment.

The encoding resolution is set by the first region and never changes. When the
region is resized mid-recording, the X11 and Wayland compositors scale the new
region on the GPU uniformly until it fits the fixed video canvas and centre it,
filling the rest of the canvas with black bars (letterboxed or pillarboxed)
rather than stretching it; the recorded cursor follows the same mapping.
Moving the region, or giving it back its original shape, still fills the whole
canvas. ffmpeg is never restarted, and the microphone track is never
interrupted. The red adjustment border shows only on the local desktop and is
never written into the video.

Automation can set the region at start and update it while recording:

```bash
jwm-tool msg start_recording \
  --args '{"path":"/tmp/demo.mp4","x":100,"y":80,"width":1280,"height":720}'
jwm-tool msg set_recording_region \
  --args '{"x":320,"y":180,"width":960,"height":540}'
jwm-tool msg get_recording_status
```
