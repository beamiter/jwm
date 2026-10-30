# Touchpad gestures

In native Wayland (udev/DRM) and supported X11 sessions, three-finger swipes switch the
selected monitor's tags by default: swipe left for the previous (left) tag and
right for the next (right) tag. The ends wrap around. This uses the same `loopview`
action as keyboard navigation, including per-tag layout and view updates.
These are JWM workspace/tag changes, not browser tabs or window tab groups.
Swipe up to minimize the selected window. Swipe down to open the minimized
window restore preview, then choose a window to restore; dismissing the preview
does not restore anything.
The restore picker shows the highlighted window's retained thumbnail beside
the list. Hover or keyboard navigation changes the thumbnail; clicking a row
or the thumbnail, or pressing Enter, restores just that window. Escape leaves
all minimized windows untouched.

Four-finger gestures manage windows without changing tags:

| Gesture | On the desktop | In window exposé |
| --- | --- | --- |
| Left / right | Preview the previous / next MRU window; lift to switch | Move the window highlight left / right |
| Up | Open window exposé | Close exposé without changing focus |
| Down | Show desktop; repeat to restore this view's windows | Return to the highlighted window |

The four-finger window switcher shows a list and the selected window's live
thumbnail (or retained thumbnail when minimized). A longer horizontal swipe
browses further through the snapshot, one additional window per threshold.
Pull back near the start or lift after a short swipe to cancel. Previewing
never changes focus, restores a minimized client or changes its geometry.
Only release commits the highlighted choice; Escape or another input owner
cancels. Alt+Tab keeps its existing modifier-release behavior.

Show desktop is a reversible minimize operation for visible applications on
the selected monitor and active tag set. Previously minimized windows stay
minimized. Repeating the gesture restores only the still-owned windows and
the original focus, leaving other tags and monitors alone. Manually restored,
retagged, moved or closed windows leave the snapshot; a failed restore remains
retryable. Snapshots last for the current WM session. `show_desktop` also works
over IPC. Opening exposé from the desktop restores that snapshot first.

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
The four-finger switcher owns its own panel until lift; exposé accepts only
its four-finger navigation/exit actions. Other modal interfaces still block
workspace and window gestures, including queued input from a previous owner.

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
  { fingers = 3, direction = "left", function = "loopview", argument = -1 },
  { fingers = 3, direction = "right", function = "loopview", argument = 1 },
  { fingers = 3, direction = "up", function = "minimize", argument = 0 },
  { fingers = 3, direction = "down", function = "restore_preview", argument = 0 },
  { fingers = 4, direction = "left", function = "window_switcher", argument = -1 },
  { fingers = 4, direction = "right", function = "window_switcher", argument = 1 },
  { fingers = 4, direction = "up", function = "toggle_expose", argument = 0 },
  { fingers = 4, direction = "down", function = "show_desktop", argument = 0 },
]
```

Use `gesture_swipe = []` inside `[behavior]` to disable WM swipe bindings.
An existing explicit binding list is preserved, not merged with defaults.
A lower positive `gesture_swipe_threshold` makes swipes more sensitive.
Swap the two arguments to reverse the direction. Three-, four- and
five-finger bindings and the existing IPC action names remain configurable.

Interactive previews are provided for horizontal `loopview` (also `loop_view`
and `loop`) and `window_switcher` (also `switcher`) bindings with argument
`1` or `-1`. Other bound actions, including
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
be enabled. When the Xorg driver omits gesture classes, JWM can use its own
gesture-only libinput context for the affected touchpads. The context never
grabs a device and leaves pointer motion, clicks and scrolling with Xorg.
Touchpads already exposed through XI2.4 are excluded from the fallback.
The gesture connection and X11 shell also retain authenticated Linux abstract
socket access when the server's filesystem socket path has disappeared. They
use the same display and Xauthority lookup as the ordinary connection.

The source/release installers ship `69-jwm-touchpad-gestures.rules`, granting
the active local seat access only to touchpad event nodes without keyboard
capability. The fallback monitors logind activity and udev hotplug directly;
inactive sessions close their device handles and cancel ongoing swipes.
It does not require membership in the `input` group or a root WM. For a manual
installation, copy the rule to `/usr/lib/udev/rules.d`, reload udev rules, then
trigger a change for touchpad event nodes. No external gesture daemon is used.

X11 briefly holds a swipe Begin to inspect its finger count. Configured
counts are accepted immediately; other counts are replayed to the original
application. Two-finger scrolling and pinch gestures are not grabbed. As on
Wayland, all swipe directions for a configured finger count belong to JWM.
An already-active application pointer grab can prevent delivery through XI2.4;
the gesture-only libinput fallback is independent of application pointer grabs.

The xcb build also links x11rb for this protocol connection because its xcb
protocol bindings do not yet describe XInput 2.4 gestures. Wayland-only
build profiles do not acquire this dependency through the gesture feature.

Run `jwm-tool msg get_gesture` to inspect the configured bindings and runtime
capability. `runtime.operational` is false when the X11 hook failed, no capable
device is present, or bindings are disabled; `runtime.reason` explains the
state. `configured_fingers` lists configuration intent, while
`intercepted_fingers` and `will_intercept` reflect current runtime support.
`window_preview_active` and `window_preview_target` identify an in-progress
touchpad window choice; `showing_desktop` reports the selected view's snapshot.
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

For the current X11 desktop, `python3 scripts/test-touchpad-gestures-x11.py --live`
creates a temporary virtual touchpad and one fixture window on an unused tag.
It checks all four directions, release-only commits, short/reversed/diagonal
cancellation, and pointer positions over the desktop and each screen edge.
It restores the original tag, focus and pointer and archives its result and
restore-picker capture under `artifacts/touchpad/live-x11`. Only creation of
the virtual touchpad uses the short-lived sudo helper. This exercises the
kernel/libinput/WM path; personal finger feel still needs the physical pad.
Add `--four-finger` to test window previews and release-only focus changes at
the same five positions, pull-back cancellation, exposé navigation/exit and
exact desktop restoration with a pre-minimized fixture. This creates two more
owned windows and requires no existing sticky applications on the test monitor.
