# Toast notifications

JWM renders transient notification cards natively in both compositors —
no external notification daemon. Cards stack in the top-right corner with
the same material styling as the modal system UI: rounded panel, gaussian
drop shadow, an urgency accent stripe, a bright title over a dimmer body,
and a fade in/out envelope. At most four cards are visible; older cards
are evicted first.

## Posting a toast over IPC

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

Volume/brightness keys, timers, and scripts can use this as a lightweight
`notify-send` replacement wherever the jwm IPC socket is available.

## Built-in events

JWM posts its own toasts for a few state changes:

- configuration reload succeeded (short, normal) or failed (critical, with
  the parse error in the body),
- screen recording stopped (with the output path).

While any toast is visible the scene keeps compositing (direct scanout and
KMS color offload resume once the last card fades out). The modal system
UI draws above toasts, and the lock screen hides them entirely.
