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

The runner switches to the last tag, creates only `JwmDemo` windows, records one MP4 per scene, verifies it with `ffprobe`, generates narration/SRT/report assets, and restores the original tag/layout on every normal, exception, SIGINT, or SIGTERM exit handled by Python. If the process is force-killed, run `bash video-demo/scripts/recover-session.sh`.

Current automated scenes are Tile, Grid, and Scrolling. Wobbly is marked `manual-review` until the pointer driver and reversible compositor-config transaction are complete; Overview is `experimental`. They are intentionally not represented as completed video coverage.
