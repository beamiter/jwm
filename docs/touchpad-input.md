# Touchpad device configuration

Native `wayland-udev` sessions can configure libinput touchpads directly.
The policy applies at device discovery, hotplug, configuration reload and
session resume. Mouse, keyboard and touchscreen devices are listed for
diagnostics, but touchpad policy never changes their settings.

All options are optional. An omitted option keeps the value the device had
when libinput introduced it. Removing a previously configured option and
reloading restores that initial value; it does not leave the old override
latched until the next login.

## Configure a laptop

Add only the settings you want to change to `config_wayland.toml`:

```toml
[input.touchpad]
tap = true
tap_drag = true
drag_lock = false
natural_scroll = true
disable_while_typing = true
click_method = "clickfinger"
scroll_method = "two_finger"
accel_profile = "adaptive"
accel_speed = 0.0
```

This is an example, not a new set of forced defaults. Hardware capabilities
differ; `get_inputs` reports unsupported options individually, while applying
the other supported settings. Invalid mode names and non-finite or out-of-range
speeds fail configuration validation before the policy changes.

| Setting | Accepted value | Meaning |
| --- | --- | --- |
| `tap` | Boolean | Tap to click |
| `tap_drag` | Boolean | Tap, then hold a finger to drag |
| `drag_lock` | Boolean | Continue a tap-drag across a short finger lift |
| `natural_scroll` | Boolean | Reverse the scroll direction in libinput |
| `disable_while_typing` | Boolean | Use libinput's supported typing suppression |
| `disable_while_trackpointing` | Boolean | Suppress touchpad activity while using a trackpoint; requires the optional `libinput-1-21` build feature |
| `left_handed` | Boolean | Swap the device's supported handedness |
| `click_method` | `button_areas`, `clickfinger` | Use clickpad areas or finger count for physical clicks |
| `scroll_method` | `two_finger`, `edge`, `none` | Select the touchpad's scrolling method |
| `accel_profile` | `adaptive`, `flat` | Choose a supported acceleration profile |
| `accel_speed` | Number from `-1.0` to `1.0` | libinput pointer acceleration speed |
| `send_events` | `enabled`, `disabled`, `disabled_on_external_mouse` | Device event policy, subject to hardware support |

JWM's default build retains the existing libinput 1.19 baseline. On a system
with libinput 1.21 or newer, enable the optional trackpoint-suppression API:

```bash
cargo build --release --features libinput-1-21
```

This feature enables the getter, capability check and setter for
`disable_while_trackpointing`, including restoration after removing an override.
It still requires device support. Baseline builds report a requested value as
`unsupported_by_bindings`; enabled builds report `unsupported` when the physical
device lacks the capability. `get_inputs` includes the actual capability and
effective getter value when the API is enabled. No custom palm or typing filter
is inserted above libinput.

## Match individual touchpads

`input.touchpad` provides shared settings. Matching `input.touchpad_rules` are
then applied in file order; the last explicitly supplied value wins. Every
selector in a rule must match. Omitted settings inherit from the preceding
policy, and an explicit `false` overrides `true`.

```toml
[[input.touchpad_rules]]
name = "ELAN"                 # Case-insensitive name substring
vendor = 0x04f3                # Optional numeric vendor ID
gestures = true                # Optional native-gesture capability selector
accel_speed = -0.15

[[input.touchpad_rules]]
identifier = "05ac:0274:Apple Inc. Magic Trackpad 2"
natural_scroll = true
disable_while_typing = false
```

Use the actual identifier returned by `get_inputs`, not the example identifier.
`identifier` compares the full stable vendor/product/name string exactly.
`product` is also available as an optional numeric selector. The transient
`eventN` device ID identifies runtime events and is not a persistent selector.

## Inspect effective settings

```bash
jwm-tool msg get_inputs
jwm-tool msg get_gesture
jwm-tool msg reload_config
```

`get_input_status` is an alias for `get_inputs`. The response includes the
configured policy plus a `runtime` section with session state and a device list.
Each device exposes:

- `id`, `identifier`, name, vendor/product IDs and input capabilities;
- `supported` methods, profiles and capability flags;
- `requested` merged overrides, `initial` values and `effective` getter values;
- `settings_results` for each setting or restoration, with `applied`,
  `unsupported`, `unsupported_by_bindings` or `invalid` status.

libinput may defer some changes until a device is neutral. `applied` means its
setter accepted the request; `effective` is the last snapshot of the configured
getter values, not proof that a physical gesture has completed. Session resume,
reload and device changes refresh the snapshot.

`get_gesture` now reports actual native gesture-capable touchpads and session
activity rather than inferring hardware availability from a compiled backend.
A device configured as fully disabled is excluded from its operational count.
The external-mouse send-events mode can still temporarily suppress a device.

X11 sessions and nested Wayland sessions report `host_managed` for these device
settings. Their X server or host compositor owns the physical libinput context.
The configuration is retained so it can take effect in a native Wayland login.

## Scrolling in JWM panels

The native Wayland input path retains both axes, fractional displacement,
source, device identity and per-axis stops. Launcher, keybinding and picker
lists move their viewport continuously without changing the keyboard's
selected action. Text, icons, the scrollbar and click hit testing use the
same fractional offset. A pointer click chooses the visible row; keyboard
navigation resumes the selection's viewport. Background list refreshes keep
the visible item and fractional position where possible.

Finger lift may continue a list with a short, decaying local fling. A pause
before lift, a list boundary, another input, a two-finger hold, a panel change
or a device removal stops it. Disabling motion in JWM also disables this
fling. There is no periodic scroll wakeup while idle, and JWM does not add
inertia to application surfaces.

Controls with discrete actions use the same default distances as the bars:
40 units for navigation and 16 for each slider/capture adjustment. Navigation
fires at most once per precision sequence; adjustments may repeat while the
finger continues moving. Horizontal scrolling remains available for grids
and navigation, but does not adjust volume, brightness or capture stroke
width. A gesture cannot transfer its accumulated distance into another
control after a pointer move or a dynamic row rearrangement.

Opening or replacing a panel, entering capture, locking, switching VTs and
reloading input policy end the old scroll recipient before ownership changes.
The remaining physical tail is suppressed until a fresh sequence, including
packets already queued for the old panel. Devices and axes keep independent
lifetimes. Nested streams that lack physical stop information use a 250 ms
idle boundary; native finger streams retain their identity through a pause.

X11's core wheel-button path retains its discrete behavior. Device setup in
X11 and nested sessions remains the host's responsibility. See the separate
[bar scrolling policy](../bars/README.md#wheel-and-touchpad-input) for toolkit
adapters and their `[presentation.scroll]` sensitivity settings.

## Hardware verification

On a native Wayland laptop, check the diagnostic before and after changing a
setting. Verify tapping, tap-drag, click method and typing suppression separately;
then test vertical, horizontal and diagonal two-finger scrolling, finger lift,
and a fresh scroll. Test an external touchpad next to the built-in one, unplug
and reconnect it, reload a rule, remove an override, and suspend/resume. Confirm
that an external mouse's settings remain unchanged. Gesture actions are
configured separately in [touchpad workspace swipes](touchpad-gestures.md).

### Preview scanout and scroll ownership regressions (PR #76 follow-up)

Native Wayland keeps direct scanout blocked for both an interactive workspace
preview and its release/cancel animation. A stationary finger does not require
continuous animation ticks, but the retained preview still requires composition.
Once `finish_transition` clears the transition and swipe state, the ordinary
fullscreen direct-scanout optimization is available again.

A bar hover change discards partial distance and fences further actions without
ending an explicitly started physical scroll. Crossing from audio to brightness,
leaving the bar, passing over empty space, or pausing longer than the implicit
250 ms idle timeout cannot transfer that gesture. End/Cancelled packets over
empty space still release its lifetime. Source-less toolkit streams also ignore
interleaved changes between pixel and line units while an explicit owner is
active; these packets cannot identify a different physical device reliably.
Backends with no explicit phases retain the idle-boundary fallback for X11 wheels.

Regression coverage lives in `xbar_core::input`, the Cairo pointer dispatch tests,
the native Wayland composition gate tests, and the WM's interleaved swipe-ID test.
For hardware validation, use a scanout-eligible fullscreen client: begin a
three-finger preview, pause while holding, reverse, then release or cancel. Check
that the preview remains visible and that direct scanout resumes after settling.
Also hold a scroll across audio/brightness, pause for one second, and resume;
brightness must remain unchanged until a new gesture begins.

Validation for this follow-up (2026-10-30):

- `cargo fmt --all -- --check`, changed standalone Bar files' rustfmt checks,
  `git diff --check`, and `cargo check --locked --all-targets` passed. The root
  default features include X11RB, XCB, native Wayland and nested Wayland.
- Shared core: 265/265 tests passed. With `render-cairo`: 297 passed and one
  existing text-color raster test failed; that exact failure was reproduced on
  unmodified master `9c8da887` in the same environment. All added input tests pass.
- JWM library: 7242 passed, 70 failed, 2 ignored. The full suite is not green:
  failures include missing Xvfb, denied Unix socket/FIFO operations and process
  inspection assumptions in this managed environment. Focused touchpad tests
  passed 8/8; native Wayland damage/composition tests passed 22/22, including
  the new fullscreen preview guard and extended interleaved-device regression.
- Physical DRM/KMS direct scanout and live touchpad acceptance were not run.
  Standalone toolkit builds and web bundle tests were not completed here (the
  web runner lacked its Playwright browser). Do not treat shared-core coverage
  as a full 28-Bar or real-device acceptance result.
