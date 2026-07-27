# Notifications

JWM is the notification service, not just its renderer. Toast cards are drawn
natively by both compositors, a bounded history keeps what scrolled past, and
`jwm-bridge` puts a freedesktop D-Bus face on the whole thing so ordinary
applications — `notify-send`, Thunderbird, Firefox — reach it without dunst or
mako running.

```
application --Notify()--> jwm-bridge --IPC notify--> jwm --> toast card
                                                        \--> history
    ActionInvoked / NotificationClosed <--IPC events----/
```

The compositor stays synchronous and D-Bus-free: the bridge is a separate
process, so a wedged session bus can never stall a frame.

## Toast cards

Cards stack in the top-right corner with the same material styling as the
modal system UI: rounded panel, gaussian drop shadow, an urgency accent
stripe, a bright title over a dimmer body, and a fade in/out envelope. At most
four cards are visible; older cards are evicted first.

While any toast is visible the scene keeps compositing (direct scanout and
KMS color offload resume once the last card fades out). The modal system UI
draws above toasts, and the lock screen hides them entirely.

## Notification center

`Alt+F11` (`notification_center`) opens the history as a material card, newest
first: urgency icon, sending application, summary, body preview, and a compact
age. Rows Do-Not-Disturb suppressed carry a DND marker, so nothing is lost
while notifications are muted.

| Key | Action |
| --- | --- |
| `Up` / `Down` | move the selection |
| `Enter` | invoke the sender's default action, then dismiss |
| `d` / `Delete` | dismiss the selected notification |
| `c` | clear the whole history |
| `Esc` | close the panel |

The history holds 64 records; the oldest is evicted beyond that. It is
in-memory only and does not survive a restart.

## The D-Bus bridge

`bridge/` is a small separate crate implementing `org.freedesktop.Notifications`
(`Notify`, `CloseNotification`, `GetCapabilities`, `GetServerInformation`, and
the `NotificationClosed` / `ActionInvoked` signals). Build and install it, then
let D-Bus activate it:

```sh
cd bridge && cargo build --release
sudo install -Dm755 target/release/jwm-bridge /usr/local/bin/jwm-bridge
install -Dm644 dist/org.freedesktop.Notifications.service \
  ~/.local/share/dbus-1/services/org.freedesktop.Notifications.service
```

Only one process may own `org.freedesktop.Notifications`; disable dunst, mako,
or a desktop environment's own daemon first.

Mapping rules, all unit tested in `bridge/src/notifications.rs`:

- the `urgency` hint maps onto jwm's 0/1/2 scale, defaulting to normal;
- `expire_timeout` `-1` takes jwm's default, `0` ("never expire") becomes the
  longest card jwm draws (30 s) while the record stays in the history;
- `replaces_id` updates a record in place, so progress notifications stay one
  row and keep their identifier;
- an action list yields a default action when it contains the reserved
  `default` key, or when exactly one action is offered — activating a row
  never guesses between several.

jwm owns the identifiers and every close, so a toast that expired, a row the
user dismissed, and a `CloseNotification` call all emit exactly one
`NotificationClosed` with the specification's reason code.

## Posting a toast directly

Scripts with access to the IPC socket can skip D-Bus entirely:

```sh
jwm-msg '{"command": "notify", "args": {
  "title": "Build finished",
  "body": "jwm 0.2.0 · 0 warnings",
  "urgency": 1,
  "timeout_ms": 4000
}}'
```

- `title` / `body` — either may be empty, not both. The body keeps at most
  3 lines; long lines are ellipsized at 80 characters.
- `urgency` — `0` low (muted stripe), `1` normal (border-gradient accent
  stripe), `2` critical (red stripe). Defaults to `1`.
- `timeout_ms` — display time, clamped to 800..30000; `0` selects the
  default 4000.
- `app`, `replaces_id`, `default_action` — optional; what the bridge forwards.

The response carries the identifier: `{"success": true, "data": {"id": 12}}`.

Related IPC: `close_notification` (`id`, optional `reason`),
`clear_notifications`, the `get_notifications` query, and the `notification`
subscription topic carrying `notification/posted`, `notification/closed`, and
`notification/action`.

## Do Not Disturb

`toggle_dnd` (also a control-center row) suppresses toast cards and unmaps
X11 notification windows from external daemons. Suppressed notifications are
still recorded, marked in the notification center, and still emit their
`notification/posted` event.

## Built-in events

JWM posts its own toasts for a few state changes:

- configuration reload succeeded (short, normal) or failed (critical, with
  the parse error in the body),
- screen recording stopped (with the output path).
