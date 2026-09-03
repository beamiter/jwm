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

Cards answer the pointer. Hovering one freezes its countdown — the timer
resumes from the frozen point when the pointer leaves, so reading a long
body never races the fade. A left-click on the card body dismisses it with
a quick 120 ms fade-out; the click is swallowed before window dispatch, so
it never falls through to the client underneath the card.

## Notification center

`Alt+F11` (`notification_center`) opens the history as a material card, newest
first: urgency icon, sending application, summary, body preview, and a compact
age. Rows Do-Not-Disturb suppressed carry a DND marker, so nothing is lost
while notifications are muted.

| Key | Action |
| --- | --- |
| `Up` / `Down` | move between notifications |
| `Page Up` / `Page Down` | move one visible page without wrapping |
| `Home` / `End` | jump to the newest / oldest notification |
| `Tab` / `Shift+Tab` | move forward / backward between notifications |
| `Left` / `Right` | move between the selected notification's buttons |
| `1` - `6` | invoke that button directly |
| `Enter` | invoke the button under the cursor, or dismiss when there is none |
| `d` / `Delete` | dismiss the selected notification |
| `c` | clear the whole history |
| `Esc` or `Alt+F11` | close the panel — the key that opened it also dismisses it |
| another panel key | hand the screen to that panel, closing this one |

Pointer input follows the same model: hover highlights a notification, click
invokes its current/default action (or dismisses an action-less row), the wheel
browses history, and clicking outside closes the card. The numbered action
strip remains a keyboard cursor target rather than pretending each glyph is a
separate pointer button.

`Up`/`Down` always move *between* rows and `Left`/`Right` always move *within*
the highlighted one — the same rule the control center and the calendar follow.

The history holds 64 records; the oldest is evicted beyond that. It survives
a restart: every change (post, replace, dismiss, clear) is written through to
`$XDG_DATA_HOME/jwm/notification-history` — `~/.local/share/jwm/` when
`XDG_DATA_HOME` is unset — and read back on startup. The file is a compact
JSON document carrying a `version` field (currently `1`) and the identifier
counter alongside the records, written atomically with `0600` permissions. A
missing, oversized, or malformed file simply starts an empty history rather
than failing startup.

## Action buttons

An application may offer actions with a notification — *Reply*, *Open folder*,
*Restart now*. The selected row shows them as a numbered strip on the line
beneath it, with a mark on the one `Enter` would invoke:

```
  [updater] Update ready — jwm 0.2.1 is available   now
       1 Later   ✓2 Restart now   3 Release notes
```

Invoking one sends the sender `ActionInvoked` and then closes the notification,
in that order, which is what the specification expects.

A toast card for a notification with actions shows up to three of them as
chips along its bottom edge, accent-outlined, with an accent wash on the
chip under the pointer. Clicking a chip invokes the action through the same
pipeline as the notification center — `ActionInvoked`, then the record
closes as dismissed — while clicking the card body still just dismisses the
toast. A click on a card that is already fading out is swallowed without
invoking anything twice. Chips follow their own sanitation: an action with
no key is dropped before the cap is counted, and a blank label falls back
to its key.

Rules worth knowing, all unit tested:

- **The cursor starts on the reserved `default` key** wherever the sender put
  it, so a notification offering one action, or an explicit `default`, behaves
  exactly as it did before buttons were drawn. Several actions with no
  `default` start on the first — the one deliberate change, and safe only
  because the strip shows which one is selected.
- **Six buttons at most, twenty characters per label.** The card is as wide as
  its widest line and the strip is one line; a client offering a dozen would
  otherwise push the panel off the screen. A `default`-keyed action beyond the
  cap displaces the last kept one rather than being dropped, since it is the
  key `Enter` runs.
- **An action with no key is dropped** — invoking it would hand the sender an
  empty string, which tells it nothing. A blank label falls back to its key,
  because a chip with no text cannot be aimed at. Repeated keys are kept, both
  of them: dropping one would slide every later label onto the wrong chip.
- **A digit beyond the offered count does nothing.**
- **Replacing a notification replaces its buttons**, so a progress
  notification that stops offering *Cancel* stops showing it.
- jwm refuses to emit `ActionInvoked` for a key the record never offered.

A notification arriving while you are choosing does not move your place: the
panel rebuild keeps the selected row and its cursor.

## The D-Bus bridge

`bridge/` is a small separate crate implementing `org.freedesktop.Notifications`
(`Notify`, `CloseNotification`, `GetCapabilities`, `GetServerInformation`, and
the `NotificationClosed` / `ActionInvoked` signals). The same process also
watches MPRIS players for the shell's [media controls](media-controls.md).
Build and install it, then let D-Bus activate it:

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
- the whole action list is forwarded in the order it was sent, which the
  specification requires to be the display order;
- `default_action` is still sent beside it, carrying the reserved `default`
  key or a lone action. It is what an older jwm reads: the bridge is installed
  separately from the compositor, so a new bridge talking to an older jwm is a
  real deployment, and a jwm new enough to read `actions` ignores the field.

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
- `app`, `replaces_id` — optional; what the bridge forwards.
- `actions` — optional; the flat `[key, label, key, label, …]` list D-Bus uses.
- `default_action` — optional legacy single key, read only when `actions` is
  absent.

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
