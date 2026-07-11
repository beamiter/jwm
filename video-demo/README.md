# JWM video automation MVP

This implementation records deterministic demo windows through JWM's own X11 compositor recorder. Formal runs deliberately require the real X11 login session; they do not fall back to Xephyr or Xvfb.

Build, install/restart the modified JWM, and check the environment. Building only
the demo client does not update the already-running window manager:

```bash
jwm-tool rebuild --jwm-dir "$PWD"
python3 video-demo/runner/run_demo.py --preflight
```

Record the ready smoke profile:

```bash
python3 video-demo/runner/run_demo.py --backend x11rb --profile smoke --build-demo-client
```

The runner always builds and launches `demo-client/target/release/jwm-demo-client`
and rejects a running JWM that does not report a release build profile.

The runner switches to the last tag, creates only `JwmDemo` windows, records one MP4 per scene, verifies it with `ffprobe`, generates narration/SRT/report assets, and restores the original tag/layout on every normal, exception, SIGINT, or SIGTERM exit handled by Python. If the process is force-killed, run `bash video-demo/scripts/recover-session.sh`.

Available release profiles:

```bash
# Fast regression: Tile, Grid, Scrolling
python3 video-demo/runner/run_demo.py --backend x11rb --profile smoke

# All 13 JWM layouts
python3 video-demo/runner/run_demo.py --backend x11rb --profile layouts

# Focus/order, mfact/cfact/nmaster, Floating, Sticky and PiP
python3 video-demo/runner/run_demo.py --backend x11rb --profile window-management

# Safe window migration to an automatically selected empty tag
python3 video-demo/runner/run_demo.py --backend x11rb --profile tags

# Complete phase-2 recording set
python3 video-demo/runner/run_demo.py --backend x11rb --profile stage2
```

Compositor scenes use reversible runtime configuration and real XTest pointer
input. They are automated but remain marked `manual_review` because visual
effect quality cannot be proven from IPC state alone.

## Post-production

Generate narration text, per-scene and global SRT subtitles, CSV timeline,
FFmetadata chapters, JSONL narration jobs, and assemble all ready scenes:

```bash
python3 video-demo/runner/run_demo.py \
  --profile production \
  --generate-assets \
  --assemble
```

An external TTS program can be plugged in without coupling the runner to a
specific model. The command must create `{output}` from `{text}`:

```bash
python3 video-demo/runner/run_demo.py \
  --profile production \
  --generate-assets \
  --tts-command 'python3 /path/to/tts.py --text-file {text} --output {output}'

python3 video-demo/runner/run_demo.py \
  --profile production \
  --assemble \
  --voice
```
