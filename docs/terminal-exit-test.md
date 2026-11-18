# Private X11 terminal-exit overlap regression

`test-terminal-exit-x11.py` owns Xvfb → Xephyr, a private session bus and private
HOME/XDG directories. Software GL is forced. It does not use a user's display,
terminal, session lock or password. Pure-color Python/Xlib producers publish
`_NET_WM_PID` and remain alive only while the private fixture marker exists.
The terminal is spawned by the actual `togglescratchpad` command and matched to
its real PID; hide/reverse input uses actual Alt+Shift+N through Xephyr's host.
Native XIDs are used only for X11 tools/focus checks. IPC rows are resolved by
producer PID and their distinct internal JWM IDs are retained for IPC focus and
scratchpad-registry assertions; the two ID spaces must never be equated.

Dependencies: Xvfb, Xephyr, xdotool, ImageMagick `import`, dbus-run-session,
libX11 and Pillow. Supply a newly built JWM binary with the terminal hide fix:

```sh
python3 scripts/test-terminal-exit-x11-harness.py -v
python3 scripts/test-terminal-exit-x11.py --jwm target/debug/jwm \
  --backend x11rb --output artifacts/terminal-exit-x11rb
python3 scripts/test-terminal-exit-x11.py --jwm target/debug/jwm \
  --backend xcb --output artifacts/terminal-exit-xcb
```

`--preflight` checks dependencies without launching a desktop. CI integration
can use the existing native restart-regression dependencies and binary build;
retain the output directory with an always-run artifact step.

The fixture requests Above, PiP and fullscreen backgrounds, then verifies their
actual managed state and reads back their geometry. It samples a conservative
interior shared by the background and the moving terminal over each capture's
time interval. The projection mirrors the terminal's 1000ms cubic ease-in-out
exit. Hide onset is bounded by key-command start and the later of command finish
and observed focus release; command return alone does not prove WM processing. A separate 80ms presentation margin is applied.
A pre-key PNG and both monotonic command timestamps are retained. Capture continues for the full 1000ms animation plus 150ms (at most 60 frames).
At least three overlapping frames must contain >90% terminal color, with explicit
early, middle and final-visible-phase coverage requirements relative to the
read-back geometry. The last checked frame must be within 75ms of the conservatively projected
last visible overlap; a post-completion frame is mandatory. Natural departure from a small PiP intersection is
not counted as occlusion; insufficient sampled overlap fails as inadequate
fixture evidence. The terminal must disappear at completion and uncover blue
background pixels. The first observed focus release must be within 200ms from the key-command
start (including dispatch and IPC observation cost, retained in JSON); native input focus
must not remain on the hiding terminal. Mid-exit reversal must preserve PID and
restore managed focus/visibility; resting stacking follows normal policy.

A full-display green override-redirect producer must cover all terminal pixels
during hide. This is **not a session-lock/password-screen test**. Wayland,
physical GPU output and input-to-photon behavior are also outside this fixture.
The OR overlay is sampled through the entire same duration, not only the first
few frames. Every sampled PNG, config, compositor/producer log and JSON timing/color report
is retained, including binary/fixture SHA-256 identities. No runtime test result
is implied by the offline harness tests or Python compilation alone.
