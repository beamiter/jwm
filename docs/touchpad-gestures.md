# Touchpad workspace swipes

In native Wayland (udev/DRM) and supported X11 sessions, three-finger swipes switch the
selected monitor's tags by default: swipe left for the next tag and right
for the previous tag. The ends wrap around. This uses the same `loopview`
action as keyboard navigation, including per-tag layout and view updates.
These are JWM workspace/tag changes, not browser tabs or window tab groups.
Swipe up to minimize the selected window. Swipe down to open the minimized
window restore preview, then choose a window to restore; dismissing the preview
does not restore anything.

With JWM compositing and animations enabled, horizontal workspace swipes
move the outgoing and incoming workspace images with your fingers. Pull back
to undo the preview; lifting after a short swipe returns it to the source.
The preview is clipped to the selected monitor's work area, leaving other
monitors and bars in place. It uses the destination windows' last visible
geometry and committed content. A tag's layout is arranged when the switch
commits, and a per-tag wallpaper changes at that point; the preview does not
request a new client layout or transfer input focus.

The monitor, source tag and selected client are captured at Begin. While
previewing, the real window mapping, tags and input geometry stay on that
source. Only release calls the usual action once. The final visual movement
or return takes 180 ms; a held gesture does not advance on a timer.

Recognition locks the main axis after 8 units and a 1.5:1 margin over the
other axis. A normal release commits at `gesture_swipe_threshold` (80 units
by default, half of the full visual travel). A quick flick can commit after
35% of that distance if its smoothed speed reaches 900 units/second in the
same direction and the last movement was within 80 ms. Tiny movements,
ambiguous diagonals and a short flick followed by a pause do nothing. A fast
movement back toward the source cancels, even beyond the distance threshold.
Directions follow net displacement, so a swipe can reverse before release.

Opening a system panel, screenshot or recording-region selector, overview,
exposé or annotation, entering lock or suspension, changing monitor/focus,
removing the device or reloading configuration cancels stale ownership.
Queued input cannot revive a gesture across that handoff. Cancellation due
to a new input owner restores the live scene immediately; an ordinary
cancelled or short release returns smoothly. A second device cannot finish
or replace the first device's active workspace gesture.

Two-finger scrolling and unclaimed swipe/pinch/hold gestures continue to
applications. A modal JWM interface owns its input instead. Native Wayland
does not perform WM swipe actions while the session is locked. A hold over
a JWM panel stops JWM's scroll inertia without forwarding that hold to the
client behind the panel.

## Configuration

The following defaults apply when `behavior.gesture_swipe` is absent:

```toml
[behavior]
gesture_swipe_threshold = 80.0

gesture_swipe = [
  { fingers = 3, direction = "left", function = "loopview", argument = 1 },
  { fingers = 3, direction = "right", function = "loopview", argument = -1 },
  { fingers = 3, direction = "up", function = "minimize", argument = 0 },
  { fingers = 3, direction = "down", function = "restore_preview", argument = 0 },
]
```

Use `gesture_swipe = []` inside `[behavior]` to disable WM swipe bindings.
An existing explicit binding list is preserved, not merged with defaults.
A lower positive `gesture_swipe_threshold` makes swipes more sensitive.
Swap the two arguments to reverse the direction. Three-, four- and
five-finger bindings and the existing IPC action names remain configurable.

Interactive previews are provided for horizontal `loopview` (also `loop_view`
and `loop`) bindings with argument `1` or `-1`. Other bound actions, including
custom IPC actions, run once at release and retain their existing behavior.
For example, `toggle_expose` can be bound to open or close exposé. Invalid or
unknown action names do not claim a finger count.

`[animation] enabled = false`, animation speed `"instant"`, a zero animation
duration, or `[behavior] transition_mode = "none"` disables the workspace
preview. Gesture recognition and the release action remain available. Without
JWM's compositor, workspace changes also execute on release without a preview.

Because ownership is decided at gesture begin, native Wayland claims all
swipe directions for a configured finger count; an unbound direction does
nothing. Other finger counts continue to clients. Nested Wayland backends
do not currently supply the native libinput swipe stream.

## X11 requirements and fallback

Both the x11rb and xcb backends use a small dedicated XInput connection in
JWM itself. The server must implement XInput 2.4 and the input driver/device
must emit swipe gestures. Older servers or unsupported touchpads keep normal
pointer and keyboard operation; JWM logs why the native swipe hook could not
be enabled. No external gesture daemon or direct `/dev/input` access is used.

X11 briefly holds a swipe Begin to inspect its finger count. Configured
counts are accepted immediately; other counts are replayed to the original
application. Two-finger scrolling and pinch gestures are not grabbed. As on
Wayland, all swipe directions for a configured finger count belong to JWM.
An already-active application pointer grab can prevent delivery of a swipe.

The xcb build also links x11rb for this protocol connection because its xcb
protocol bindings do not yet describe XInput 2.4 gestures. Wayland-only
build profiles do not acquire this dependency through the gesture feature.

Run `jwm-tool msg get_gesture` to inspect the configured bindings and runtime
capability. `runtime.operational` is false when the X11 hook failed, no capable
device is present, or bindings are disabled; `runtime.reason` explains the
state. `configured_fingers` lists configuration intent, while
`intercepted_fingers` and `will_intercept` reflect current runtime support.
An application's active grab can still temporarily prevent delivery.

## Validation

Automated coverage includes axis locking, distance and flick decisions,
direction reversal, timestamp wrapping, cancellation and duplicate End;
WM target/focus validation, parked-window preview geometry, modal takeover
and queued Begin fencing; and a headless GLES pixel test of both moving
workspace images and monitor clipping. These tests exercise code and render
paths without emulating physical touchpad characteristics. Check threshold,
flick feel, device removal, VT resume and fractional-scale multi-monitor
behavior on the actual laptop before selecting personal sensitivity values.
