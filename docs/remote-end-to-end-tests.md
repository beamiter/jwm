# Remote failure integration tests

```sh
cargo build --locked --no-default-features --features remote-x11 --bin jwm-remote
python3 scripts/test-remote-e2e.py --binary target/debug/jwm-remote --output /tmp/jwm-remote-e2e
```

The test requires Python 3, Xvfb, libX11, xdotool and xrandr. Missing tools
fail the run rather than silently skipping it. It creates two private Xvfb
servers using `-displayfd`, a temporary private key, a loopback TCP forwarding
proxy, and the real `jwm-remote host` and `connect` processes. It never connects
to the current desktop and deletes its key at exit. JSON results and per-case
logs remain in `--output`; do not reuse the same output directory concurrently.

The eight cases exercise abrupt transport loss during a held key, a suspended
slow viewer with changing host pixels, view-only keyboard and pointer policy,
an external X11 keyboard grab, static viewport resizing, an unresponsive
clipboard selection owner, removal of the selected RandR monitor, and normal
input/session teardown. The host remains running after each disconnect: its
thread count must return to its pre-session baseline. Slow-viewer checks cover
the actual 600ms release message, bounded RSS growth and the two-frame credit
limit. Timeouts allow CI scheduling slack; they are not latency benchmarks.

The capture source is the supported **root fallback**. This validates real
XTEST, protocol, encoder, viewer and worker lifetimes, but does not validate
JWM's Composite overlay lease, a hardware keyboard grab, real network jitter,
DRM/KMS, Wayland capture or physical monitor hotplug. Keep the existing remote
unit suite and run the real-machine checklist in `docs/remote-control.md`.
No automatic reconnection behavior is introduced.
