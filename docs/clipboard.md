# Clipboard history

`Alt+Ctrl+V` (`clipboard_picker`) lists what was copied, newest first, and
puts the entry you choose back on the clipboard.

| Key | Action |
| --- | --- |
| `Up` / `Down` | move the selection |
| `Enter` | copy the entry back |
| `d` / `Delete` | forget the selected entry |
| `c` | clear the whole history |
| `Esc` or `Alt+Ctrl+V` | close — the key that opened the list also dismisses it |

Each row shows its position, how much was copied (`31c` for a single line of
31 characters, `3L` for three lines), and a one-line preview with whitespace
collapsed.

Copying something already in the history moves it back to the top instead of
adding a duplicate — the list is "what I might paste next", so recency is the
useful order.

## Privacy

This is a feature that remembers what you copy, so what it *refuses* to
remember matters as much as what it keeps:

- **Nothing is written to disk.** The history is in memory only and does not
  survive a restart. A clipboard manager that persisted passwords to a file
  would be a liability, not a feature.
- **Offers marked as secrets are never recorded.** Password managers tag the
  clipboard with `x-kde-passwordManagerHint`; JWM drops those before reading
  the payload, so the password never reaches the compositor's memory at all.
  `application/x-secret` and `x-secret` are honored the same way.
- **The IPC hands out previews, not contents.** `get_clipboard` returns
  truncated, whitespace-collapsed previews — a compromised IPC client cannot
  ask for every password you have copied in one request.
- **Payloads over 256 KiB are ignored**, and non-text offers (images, file
  lists) are never captured.

Turn it off entirely with:

```toml
[behavior]
clipboard_history = false
```

With it off, nothing is recorded and the picker refuses to open.

## How capture works

On X11 the clipboard is not storage but a protocol: the copying application
keeps the data and hands it over on request. JWM therefore watches CLIPBOARD
ownership through XFIXES, asks each new owner for its target list, and only
requests the payload when that list is text and carries no secret marker.
Putting an entry back means *becoming* the owner and answering requests for
as long as JWM holds the selection.

On Wayland JWM *is* the selection owner's counterpart: a client setting the
clipboard is announced through smithay's selection handler, and JWM asks that
client for the text over a pipe. The announcement arrives one step before
smithay records the selection, so the read is deferred by one turn of the
event loop — asking inside the handler always answers "no selection". Reading
and writing both happen on threads: the other end is another process that
writes at its own pace, and a compositor that waited would stall every client
with it.

The X11 watcher runs on **its own X connection and thread**, not the window
manager's.
Selection traffic is a conversation with other clients — a conversion waits
for the owner to answer — and a slow or hostile clipboard owner must never be
able to delay a frame.

Support by backend:

| Backend | Capture | Serve |
| --- | --- | --- |
| `x11rb`, `xcb` | yes | yes |
| `wayland-udev`, `wayland-x11`, `wayland-winit` | yes | yes |

Every backend is wired. A backend that could not start its watcher logs the
reason and runs without a history rather than failing the session; activating
a row then reports that the backend cannot set the clipboard rather than
pretending to have done it. Entries can still arrive through
`clipboard_record`.

## IPC

```sh
jwm-msg '{"query": "get_clipboard"}'
jwm-msg '{"command": "clipboard_copy", "args": {"index": 1}}'
jwm-msg '{"command": "clipboard_record", "args": {"text": "hello"}}'
jwm-msg '{"command": "clear_clipboard"}'
```

`clipboard_copy` puts a history entry back on the clipboard by index — the
same thing the picker's `Enter` does, available to bars and scripts.

`clipboard_record` is how a backend helper or a script feeds the history;
callers are responsible for dropping secret-marked offers before calling it.
The `clipboard` subscription topic carries `clipboard/changed` whenever the
history actually changed.
