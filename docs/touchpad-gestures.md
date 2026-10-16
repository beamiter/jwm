# Touchpad workspace swipes

In native Wayland (udev/DRM) and supported X11 sessions, three-finger swipes switch the
selected monitor's tags by default: swipe left for the next tag and right
for the previous tag. The ends wrap around. This uses the same `loopview`
action as keyboard navigation, including per-tag layout and view updates.
These are JWM workspace/tag changes, not browser tabs or window tab groups.
Swipe up to minimize the selected window. Swipe down to open the minimized
window restore preview, then choose a window to restore; dismissing the preview
does not restore anything.

Release your fingers to commit. A gesture must travel at least 80 units on
its main axis, with a 1.5:1 margin over the other axis. Short or ambiguous
diagonal swipes, and cancelled gestures, do nothing. Each gesture switches
at most once. Two-finger scrolling and pinch gestures retain their existing
application handling. The native Wayland path does not perform WM actions
while the session is locked.

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
