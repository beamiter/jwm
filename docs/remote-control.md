# JWM-to-JWM remote control (X11 LAN MVP)

## Third-party remote desktop shortcuts

When Sunlogin / 向日葵, RustDesk, a VM viewer, or another remote client does
not grab the keyboard itself, local JWM bindings win before the client sees
them. On the **local** machine, focus the remote session window and press
**Ctrl+Super+F12** (Super is the Win key) to enable keyboard shortcut passthrough. JWM releases its
keyboard shortcut grabs except for the toggle, letting the client receive
combinations such as Alt+J, Alt+1, Alt+Shift+Return and chord leaders.

Press the same key again to restore local shortcuts. Focusing another window
or the desktop, closing the owner window, or restarting JWM also ends the
mode. Returning to the remote window does not re-enable it automatically.
Window withdrawal or destruction restores local shortcuts before teardown
and refocusing, including a client crash or `kill -9`, even if refocusing fails.
The toggle is reserved locally during passthrough. Built-in lock and modal
shell surfaces retain their input ownership. Mouse shortcuts are unchanged.

This works through the shared policy for X11rb, XCB and the Wayland backends.
The remote application still needs to forward the keys it receives; this
mode cannot enable an application's own disabled keyboard-forwarding option.

Older configs get Ctrl+Super+F12 in memory if that key and chord leader are free
and no valid direct toggle is configured. To choose another key:

```toml
[[keybindings.keys]]
modifier = ["Mod4", "Shift"]
key = "F12"
function = "toggle_shortcut_passthrough"
```

A directly reachable toggle binding is required to enter passthrough, even
when invoking `jwm-tool msg toggle_shortcut_passthrough`. A chord-only toggle
cannot release the mode because its leader is forwarded. A config reload
keeps passthrough with the updated exit key, or restores local shortcuts if
no exit remains.

## Built-in JWM remote control

The initial X11 compatibility target shares one JWM X11 desktop with another JWM X11 session. It is
implemented once for the X Composite surface used by both the `x11rb` and
`xcb` backends. Scoped native Wayland capture and viewing were added later (see below). Windows/macOS and other Linux window
managers are outside this first compatibility target.

The helper is a separate process: X11 readback, JPEG encoding, a slow peer,
and a broken network stay outside JWM's compositor event loop. With
XRender 0.10+ and the Composite overlay, the host downsizes the final desktop
in the X server and reads back only the encoded dimensions. This keeps large
roots from dominating the capture path. The host includes JWM effects and
system UI, while an independent XTEST connection injects authenticated input.

Managed release bundles keep the helper in their versioned `current` tree.
Add that directory before using the commands below (source installs already
place the helper in `/usr/local/bin`):

```bash
export PATH="/usr/local/lib/jwm/current/bin:$PATH"
```

## Quickest: over SSH

If you can already SSH to the other machine as the user logged in to its JWM
session, nothing needs to be set up there beyond installing the same
`jwm-remote` version.

Both helpers must also use the same application protocol. Builds can both print
`jwm-remote 0.2.0` while speaking different protocols; compare the `Local protocol`
line from `jwm-remote doctor` on each machine. An older host may report
`client hello has an invalid length` for a newer viewer with a full keymap.
Update both helpers together in that case.

```bash
jwm-remote ssh user@desktop --grab-input
jwm-remote ssh user@desktop --grab-input --monitor DP-0   # one monitor
jwm-remote ssh user@desktop --grab-input --clipboard      # copy and paste both ways
```

What it does:

1. Generates a fresh 256-bit key for this one session and hands it to the
   other machine on the SSH channel's standard input (`host --key-stdin`).
   No key file is written, copied or kept.
2. Starts `jwm-remote host` there on a random loopback port, for one session
   (`--once`), with input unless `--view-only`, clipboard only with
   `--clipboard` (text, images and files; see
   [Clipboard sharing](#clipboard-sharing)). Nothing listens on the LAN.
3. Finds the display of your running JWM session there (`--display auto`):
   the `DISPLAY` and `XAUTHORITY` of the oldest `jwm` process you own. An SSH
   login has neither, and a gdm session on `:1` with its cookie under
   `/run/user/<uid>/gdm` is not guessable.
4. Forwards a local loopback port to that host with `ssh -L` and opens the
   viewer on it. The window title names the machine:
   `user@desktop (DP-0) - JWM Remote - keyboard + pointer; ...`.

Closing the viewer, Ctrl+C or killing it closes the SSH channel, and a host
started with `--key-stdin` stops when its standard input closes. On a dropped
network the remote sshd may hold the channel open for much longer, but the
session's own idle timeout ends it within seconds and the one-session host
exits with it. Either way nothing stays running on the other machine.

This grants nothing new: the SSH login can already run any program as that
user, including a host with `--allow-input`. The session's traffic is
encrypted twice. SSH also provides its independently authenticated channel;
the native transport now has ephemeral forward secrecy of its own.

### One key for a machine you use often

Bind the command in `config_x11.toml`, one key per monitor if you like:

```toml
[[keybindings.keys]]
modifier = ["Mod4"]
key = "r"
function = "spawn"
argument = ["jwm-remote", "ssh", "user@desktop", "--grab-input", "--monitor", "DP-0", "--notify"]

[[keybindings.keys]]
modifier = ["Mod4", "Shift"]
key = "r"
function = "spawn"
argument = ["jwm-remote", "ssh", "user@desktop", "--grab-input", "--monitor", "DP-2", "--notify"]
```

A machine has one session at a time, because one host captures a screen at a
time; a lock in `$XDG_RUNTIME_DIR` records which monitor it shows. Pressing
the key of the monitor already shown brings its window to the front. Pressing
another monitor's key **switches**: the open viewer is closed the way its close
button does it, its host stops, and the new monitor opens — about 1.2 seconds
on a LAN. A press while a session is still connecting does nothing.

While the viewer has the keyboard grabbed, every key goes to the remote
machine, these shortcuts included. Press Ctrl+Super+F12 to release it first, then
press the other monitor's key. `--notify` reports a failure
(`ssh could not connect or log in`, a busy desktop, ...) as a desktop
notification, since a shortcut has no terminal. A shortcut cannot answer a
password prompt, so use a key without a passphrase or one loaded in an agent
that JWM's environment can reach.

The helper is looked up in the remote `PATH`, then in the managed bundle at
`/usr/local/lib/jwm/current/bin`. Use `--remote-command PATH` for another
location (a leading `~/` is the remote home), and `--ssh-option OPTION` for
extra `ssh -o` options such as `Port=2222`; your SSH configuration and agent
apply as usual, and password or host-key prompts appear in the terminal.

Failures say where they happened: `ssh could not connect or log in`, `jwm-remote
is not installed on this machine`, or `did not accept the host options` when
the other machine's helper is older than this one. Run `jwm-remote doctor` over
SSH on the other machine to see which session display it would share:

```text
JWM session: DISPLAY ":1" (pid 3035859); `host --display auto` and `jwm-remote ssh` use it
```

## Quick start: two X11 JWM machines

Start on **both** machines with:

```bash
jwm-remote doctor
```

This is an **offline preflight**, not a connection test. It prints the local
protocol versions, session/display environment and key-file metadata, without
opening X11 or a socket and without reading key contents. Missing display/key,
unsafe key metadata (including a missing owner-read bit) or an advertised Wayland session gives a nonzero exit.
Composite, XTEST, keyboard compatibility, monitor selection and the peer remain
explicitly **not probed**. A metadata pass does not prove that the key is valid,
readable or equal on both machines.

All commands now share a default key path: absolute
`$XDG_CONFIG_HOME/jwm/remote.key`, or `$HOME/.config/jwm/remote.key` when
`XDG_CONFIG_HOME` is unset/empty. Relative configuration/home paths and a missing
HOME are errors, never a fallback into the working directory. Environment values and key paths
are printed with visible escaping so they cannot forge diagnostic lines. Existing explicit
`--key-file PATH` (and keygen `--output PATH`) still take precedence.

For a first setup, create the private parent directory and generate a key
**once on one machine**, then copy that same key securely to the other machine
as described under [First LAN connection](#first-lan-connection). For the HOME
default (when `XDG_CONFIG_HOME` is not set):

```bash
mkdir -p -m 700 "$HOME/.config/jwm"
jwm-remote keygen
```

If `XDG_CONFIG_HOME` is set, create its `jwm` directory instead; it must be an
absolute path. `keygen` refuses to overwrite an existing key and reports a
missing parent. `keygen` and `doctor` do not create parent directories or repair
permissions.
Do not generate two different keys for the same connection. Run `doctor` again
after copying the key; its output never contains key bytes.

Start with viewing only:

```bash
# Host: sharing this machine's desktop on your trusted LAN
jwm-remote host --listen 0.0.0.0 --allow-lan

# Viewer: replace HOST with that machine's actual LAN hostname or IP
jwm-remote connect HOST --view-only
```

The default port is 48221: `HOST`, `HOST:48221`, `[::1]`, and `[::1]:48221`
are accepted. `host --listen` now accepts the same omitted-port convenience:
`jwm-remote host --listen LOCAL_IP --allow-lan` uses port 48221. `LOCAL_IP` must
be an address belonging to the **machine running the host**, not the viewer's
address. A hostname is also accepted; the parser does not look it up, and the
existing system bind operation resolves it when the host actually starts.
A bare IPv6 address always uses the default port; bracket it to specify another
port. Existing explicit ports and IPv6 scope-zone spelling are passed through
to the system bind. For example, `--listen '[::1]:48221'` is explicit IPv6.
The host also preserves explicit port `0`: the OS chooses an available port,
and the viewer must use the actual port printed by the host. `connect` still
rejects port 0.

`host` still binds `127.0.0.1:48221` by default. A listener resolved to a
non-loopback address still needs `--allow-lan`; omitted ports never grant that
permission. None of these conveniences opens a firewall or enables LAN access.
Restrict the host firewall to the intended trusted subnet yourself.

To control the machine, stop/restart the host with `--allow-input`, then run:

```bash
jwm-remote connect HOST --grab-input
```

Click the viewer to take control. Press **Ctrl+Super+F12** to release input;
close the viewer to disconnect. Use `--escape-key Pause` for Ctrl+Super+Pause if
needed. Clipboard stays off unless the host has `--allow-clipboard` **and** the
viewer has `--clipboard`.

### Know what is connected

The viewer terminal reports connecting, authentication/negotiation, waiting for
the first frame, and finally connected **after a frame is drawn and ACKed**. This is the existing viewer draw/ACK
contract, not a guarantee that a physical display has completed scanout.
The title shows actual negotiated permissions: view-only, pointer-only when
keymaps differ, or keyboard + pointer. It also shows clipboard ON/off — with a
copy in flight, its progress (`clipboard ON - receiving 3 files 40%`) and then
`3 files clipboard update queued` — and, with `--grab-input`, whether input is released
or grabbed and how to release it.
An authenticated connection waiting on capture is not reported as a connected
desktop. View-only and host input denial remain view-only; there is no fallback
that enables input automatically.

### Listener and connection errors

- The host opens its X display before it listens and prints what it shares,
  for example `sharing the whole 5120x1440 root`. A display it cannot open
  fails there, as `cannot open X11 display ":1": ...`, instead of after the
  first viewer has authenticated. When that still happens mid-run, the viewer
  is told `host cannot capture its display` rather than seeing a hang-up.
- A stale-socket cleanup can delete `/tmp/.X11-unix/X1` while the X server
  keeps running on its Linux abstract socket. libxcb tools such as `xdpyinfo`
  carry on, but x11rb does not try the abstract socket, so every helper
  connection was refused. `jwm-remote` now falls back to the abstract socket
  with the same authorization, as JWM's bars already did.
- An invalid `--listen` value fails during argument parsing, with an example
  of a local IP/name and optional port. Empty hosts, broken IPv6 brackets,
  invalid ports, whitespace and control characters are rejected before startup.
- `AddrNotAvailable` means the requested listener address cannot be assigned
  here. Check the host machine's actual interface address; using the viewer's
  address on the host is a common cause. No automatic interface probe is made.
- `AddrInUse` means another listener already uses that address/port. Stop only
  the intended existing host, or choose another port and use it in `connect`.
- `PermissionDenied` at bind means the OS denied the operation. Check port
  privileges or local security policy; `--allow-lan` does not grant OS rights.
  Do not run the desktop helper as root to bypass the denial.
- Other bind failures retain the original OS error and are not automatically
  called firewall failures. A successful SSH connection tests the SSH port,
  not the remote-desktop listener or its TCP port.

Listener errors, client TCP-connect errors, authentication/negotiation errors
and negotiated input restrictions remain separate. A non-loopback permission
refusal still requires an explicit `--allow-lan`; input still needs the host's
`--allow-input`, and a keymap mismatch still disables the keyboard. No error
silently opens LAN access, enables control or retries with weaker permissions.

### Recover deliberately

Errors identify the stage (local setup, TCP connect, negotiation, first-frame
receive/decode, local clipboard setup, local display/draw, first-frame ACK, or active session) and give the corresponding next check. A refused negotiation
may mean a busy host, wrong key or incompatible helper; it is not blindly
reported as a firewall problem. Read the specific cause printed with the hint.

A host that hangs up says nothing, so the viewer explains a hang-up by where it
happened, instead of quoting Rust's `failed to fill whole buffer`:

| Viewer says | Meaning |
|---|---|
| `host closed the connection during authentication; the two key files most likely differ` | The host never proved it holds the same key. Compare `sha256sum` of the key on both machines. |
| `host accepted the key but closed before acknowledging the session` | The key is right; an older host, or a host-side error in its terminal. |
| `host cannot capture its display: ...` | Authenticated, but the host could not open its X display; the reason follows. |
| `another viewer is already connected to this host` | Close the other viewer first. |
| `host closed the session: the host stopped sharing its desktop` | The host was stopped (Ctrl+C or SIGTERM) and said goodbye. |
| `the connection closed without a goodbye from the host` | The host process was killed or the network dropped. |

Reconnect is off by default. After a disconnect, inspect both terminals,
verify the intended host/key and the selected monitor or region, then rerun the
same `connect` command from shell history. A new connection authenticates and
negotiates permissions again. Do not put key contents into a shell command or a
reconnect script.

Direct connections can explicitly opt into a small retry budget:

```sh
jwm-remote connect HOST --view-only --reconnect-attempts 3
```

`--reconnect-attempts` accepts 0–5 additional sessions (default 0). It retries
only an unexpected EOF/reset/abort observed by the authenticated session's
network reader, after the old receiver, sender and clipboard workers have
successfully stopped. Each retry opens a new window and repeats normal key
loading, authentication and permission negotiation. The original command's
permission flags remain unchanged. Delays are 1, 2, 4, 8 and 8 seconds; the
budget never resets during the command. Ctrl+C stops the command, including
while waiting between attempts.

Startup/connect failures, authentication/protocol/decode failures observed by
the viewer, frame-idle and socket timeouts, explicit host Close/SessionClose,
local X11/clipboard errors, and any incomplete local cleanup never retry. Sender-side failures also stop conservatively,
even when the underlying cause might be network loss. Closing the viewer never
retries. Using Ctrl+Super+F12 (or the configured escape shortcut) still only
releases the grab; it also disables reconnect for that session so a later drop
cannot unexpectedly reopen a grabbed viewer. Explicitly reported layout/selection
rejections and identity/authentication failures do not silently retry or
downgrade. Every replacement connection repeats normal permission negotiation.
A bare EOF/reset is ambiguous: it can also result from a host failure whose
best-effort terminal record could not be delivered. Client cleanup certification
does not prove the host stopped only because of network loss. A fresh connection reapplies an explicit `--window-size`, if supplied, but does
not restore observed window geometry or clipboard transfer progress. SSH and saved profile runs never reconnect automatically;
nonzero reconnect consent cannot be saved in a profile.

Offline tests cover classification, budgets, cleanup and a loopback EOF path.
Live X11 reconnection and two-machine network-loss acceptance remain unverified.

### Short real-machine acceptance checklist (not run by offline tests)

1. On both X11 JWM machines, run `doctor`; confirm protocol versions match,
   metadata is private, and native/peer checks are marked not probed. A missing
   key or a relative XDG_CONFIG_HOME should fail without creating any file.
2. Start a view-only LAN host and connect with the short hostname/IP command.
   Confirm the first desktop appears before the connected line, and input and
   clipboard remain disabled.
3. Restart the host with `--allow-input`; connect with `--grab-input`. Confirm
   click-to-control, the grabbed/released title changes, and Ctrl+Super+F12 releases
   all remotely held input. With differing XKB maps, verify pointer-only status.
   In both modes, hold F12 past the keyboard repeat delay and double-tap it:
   control must stay grabbed. Ctrl+Super+F12 must release it in either mode,
   including with Caps Lock/Num Lock on or a mouse button held. Pointer-only
   keys stay local.
4. Close the viewer normally, then reconnect deliberately. Interrupt the host
   or network and confirm a clear session-stage reason; no reconnect by default.
   Separately test an explicit direct retry budget: close/escape must suppress
   retries, a clean transport loss may retry, and failed cleanup must stop.
5. Select a monitor/region, change its position/size, and verify fail-closed
   disconnect. Recheck the area before rerunning the connection command.
6. Verify a wrong key, older peer and occupied host fail safely. Enable clipboard
   on one end only, then both ends using harmless text, a screenshot and a
   copied folder in the file manager, each way; confirm its title and actual
   behavior agree. Restore clipboard-off for normal use if not needed.

## Security boundary

Remote control is disabled until `jwm-remote host` is started. The host listens
only on `127.0.0.1` by default. A non-loopback listener requires the explicit
`--allow-lan` flag, and input requires the separate `--allow-input` flag.

Every session uses a 256-bit pre-shared key, a fresh ephemeral
`Noise_NNpsk2_25519_ChaChaPoly_SHA256` handshake, independent traffic keys,
strictly increasing record sequence numbers, and ChaCha20-Poly1305 on every
frame/input message. Shared key possession authenticates a peer, not a distinct
person or device identity.
The key is never sent over the network. Key files must be owned by the current
user, must not be symlinks, and must have no group/other permission bits.
Both peers enforce a total negotiation deadline, so slowly dripping handshake
or capability bytes cannot hold a connection open indefinitely. The host also
gives each complete authenticated video record, including its final flush, one
10-second write budget. Reading a few bytes at a time cannot restart that
budget; a partial or timed-out record closes the whole session so its wire
sequence can never be retried out of sync.

The released `02f` baseline uses application version 29 and passed all four
mainline CI workflows. The next candidate uses version 30; its unified-control
PNG path has incomplete first-run CI results and awaits a corrected rerun (see
[the bounded validation summary](remote-pointer-lossless-preparation.md#unified-control-plus-png-v30-candidate)). For that candidate, earlier versions are
deliberately incompatible, so update `jwm-remote` on both machines together;
negotiation rejects an older peer before screen or input data is exchanged.
Version 7 introduced initial viewport and explicit stream selectors. Version 8
adds an independently negotiated cursor plane and a video geometry epoch;
it does not silently reinterpret v7 video or cursor selector semantics.

Historically, the v19 hello prefix is the existing 8-byte magic, big-endian `u16` version,
and capability flags. Both hellos append three one-byte selectors in order:
codec `1` (JPEG tiles), cursor `1` (composited) or `2` (independent), input `1`
(X11 keycodes). A viewer advertises independent cursor only after verifying local XRender
ARGB32 support and also accepts composited fallback. A host with XFixes and exactly one X screen/root selects independent when
requested; otherwise it
acknowledges composited video. Cursor is independent of input permission, so
view-only viewers also see host-local cursor movement.
Version 9 appends a big-endian `u64` aggregate clipboard file-byte limit after
the selectors in both hellos. The viewer offers its limit; the host acknowledges
the smaller of its own and the viewer's limits. Zero disables files. The host
acknowledges zero if file sharing is not enabled. Requested clipboard startup
failure instead ends the session and disables replacement admission until host
restart, because its cleanup cannot be confirmed. The viewer rejects an enlarged limit, a nonzero limit with files disabled,
or a zero limit with files enabled before starting clipboard workers. Both peers
apply the selected limit to outgoing manifests and incoming transfers. Text and
images retain their existing fixed bounds.

Version 10 then appends decoded-canvas limits: big-endian `u16` maximum width,
`u16` maximum height and `u64` maximum pixel count. All are nonzero and cannot
exceed 16,384 per dimension or 67,108,864 pixels. The host acknowledges the
componentwise minimum of both peers; the viewer rejects any enlarged limit.
Capture applies the selected bounds before the first frame and on subsequent
resizes, preserving aspect ratio. Decoder validates each canvas before allocation.
These limits do not change the shared source region or input authorization.

Version 12 appends two one-byte clipboard selectors after the canvas limits:
content `0` (off), `1` (text), `3` (text and images), or `7` (text, images and
files), followed by direction `0` (off), `1` (viewer to host), `2` (host to
viewer), or `3` (both). The host selects the intersection of the viewer's
request and host policy. Empty intersection disables clipboard entirely.
Flags, selectors and file limit must agree; unknown selectors and enlarged
acknowledgments fail before viewer clipboard startup. Transport crypto is unchanged.

`host`, `connect` and `ssh` accept `--frame-max-width`, `--frame-max-height` and
`--frame-max-pixels`; defaults retain the existing hard ceilings. SSH applies them
to both ends. Saved profiles preserve explicit limits; old profiles lacking the
fields use the existing hard ceilings. For example:

```sh
jwm-remote connect HOST --view-only --frame-max-width 1920 --frame-max-height 1080 --frame-max-pixels 2073600
```

This is a decoded-canvas budget, not a total-memory or source-readback budget.
JPEG atlases still include bounded tile/last-row padding and decoder scratch.
Atlas extent is checked against the canvas-derived dirty tile count and tile edge
before JPEG pixel allocation; the existing independent hard bounds still apply.

Version 15 appends one boolean byte for explicit native window resize after
clipboard selectors. The host enables it only with `--window`, `--once` and
`--allow-window-resize`; it remains independent of input and clipboard.
Kind 23 is a 28-byte request: request ID, source epoch and geometry epoch
(big-endian u64), then target width/height (big-endian u16). Kind 24 is a
37-byte result: those three IDs, observed geometry epoch (u64), actual
width/height (u16), and status byte Applied=1, Unchanged=2, Denied=3,
Stale=4, Busy=5, Failed=6. All IDs/epochs are nonzero and dimensions bounded.
See [window viewing](remote-window-view.md) for lease and confirmation semantics.

Version 16 adds the independent Wayland keyboard lease selector and records
25–28. Version 17 appends another boolean selector for the independent
Wayland pointer lease. It is not the legacy pointer permission: it admits only
single-surface, fresh-frame-gated absolute motion and atomic left clicks.
Pointer request/status/action/revoke records (kinds 29–32) are respectively
24/33/45/8 bytes. Keyboard and pointer lease modes are mutually exclusive.
See [Wayland source safety](remote-wayland-source.md) for explicit opt-in,
recipient restrictions, frame lifetime and unsupported mapping arrangements.

Version 18 appends the independent Wayland scroll boolean. Version 19 appends
one `scroll_copy` boolean for video-only copy deltas, independent of every input
permission. The bounded stream capability block is now 30 bytes; the host may
enable copy deltas only when the viewer advertises support and the operator
explicitly passes `--scroll-copy`.

The client then appends big-endian `u16` viewport width and height, followed
by the existing bounded keymap table. `(0, 0)` means no initial size; exactly
one zero dimension is invalid. The v19 server hello is exactly 41 bytes. Unknown
selectors, trailing bytes and incomplete keymaps are rejected. Input and
clipboard permission flags remain independent of the stream profile.

Compatibility: v19 viewer/v19 host works; v6/v7/v8/v9/v10/v11/v12/v13/v14/v15/v16/v17/v18 with v19 in either direction is
rejected during negotiation. There is no automatic legacy-version fallback.
Keep both binaries on the same release; this change does not alter transport
crypto. Within v19, unsupported independent-cursor capability falls back to
composited video explicitly in HelloAck.

Version 14 adds viewer-to-host `ClipboardRestrict` (kind 21) and host-to-viewer
`ClipboardRestricted` (kind 22). Each payload is exactly 10 bytes: record
version `1`, a big-endian `u64` revision, and one content-mask byte. Masks are
`7` (all), `3` (text and images), `1` (text), or `0` (off). Revisions start at
one and increase exactly once per strict subset request. Initial directions
never change. Invalid, expanding, overlapping, duplicate or out-of-order
records fail closed. Only one restriction may await confirmation.

The host ACK follows its transfer worker, actual native-backend fence and
ordered clipboard stream. The viewer also waits for its own worker/backend
fence and actual outgoing request write. A native fence has a five-second
worker wait; the viewer's overall confirmation timeout is ten seconds. Missing
confirmation ends the session without restoring sharing. After any runtime
restriction, automatic reconnect is disabled, even after an otherwise clean
transport loss: a fresh explicit connect must renegotiate permission.

Clipboard transfer opcode `7` is `Skip`: `id` (big-endian `u32`) followed by a
big-endian `u64` omitted-byte count from 1 through 128 KiB. Final outbound
dispatch substitutes it for already-counted, now-forbidden Data. It carries no
clipboard content, consumes the same bounded in-flight credit and cumulative
ACK accounting, and never feeds a sink or completes an offer. It abandons a
matching incomplete offer. This prevents cancelled data from leaking or
silently corrupting credit for subsequent permitted transfers. All previously
transmitted bytes, exposed files, history and completed pastes remain outside
the restriction's revocation guarantee.

Version 13 adds authenticated host-to-viewer `SessionClose` records (kind 20),
without changing the hello layout or transport version. Their payload is a
one-byte record version (`1`), one-byte code, big-endian `u16` reason length,
and exactly that many UTF-8 bytes, capped at 240 bytes. Codes are `1` busy,
`2` admission disabled, `3` capture unavailable, `4` clipboard unavailable,
and `5` host stopping. Unknown versions/codes, invalid UTF-8, oversized,
truncated or trailing data fail closed.

A host may send this record after validating the current ClientHello, including
before HelloAck. Invalid or older hellos still receive legacy text Close so
version mismatch remains readable. Viewers handle both records during
negotiation, initial-frame wait and an established session. Legacy text never
acquires a code by matching words. A host rejects SessionClose in the reverse
direction. Every peer termination remains terminal: no code authorizes retry
or proves local cleanup succeeded. Human reasons are sanitized for display;
command diagnostics include only the fixed code, never the reason text.

A version-5 or later host tells an older viewer which version it speaks instead
of hanging up on it, and a version-5 or later viewer reports an older host's
hang-up as "accepted the key but closed before acknowledging". Version 6 adds
chunked clipboard image and file transfers and a separately negotiated
file-sharing capability (see [Clipboard sharing](#clipboard-sharing)); a
version-5 peer would end the session on the first such record. Version 5
replaced the single whole-keymap digest in the viewer's hello with a
per-keycode table (see [Keyboard layouts](#keyboard-layouts)). Version 4 replaced whole-frame JPEG video with dirty-tile delta coding on the
same frame message; a version-3 peer would misread the first frame body, so the
handshake fails closed instead. Version 4
retains acknowledgements only for frames successfully drawn by the viewer,
which bounds host work and end-to-end video backlog. It carries input in
authenticated batches of 1–128 operations with a separate 641-byte payload
limit. The client coalesces only adjacent pointer positions (latest wins); a
key, button or release-all edge always ends that pointer run. Before causing
any input side effect, the host decodes and validates the complete batch,
preflights negotiated capabilities, the keyboard mapping and every XTEST
operation, then queues the batch in order and flushes XTEST once.
Authenticated record storage is reused only inside its owning network thread.
The receiver validates the declared length limit before growing that storage
and clears it on every truncated, malformed, or unauthenticated record; message
contents and kinds reach callers only after the tag verifies. The host declares
a 1 KiB inbound ceiling rather than the global 32 MiB frame limit, because it
only ever receives a hello, empty heartbeats, eight-byte acknowledgements and
input batches — an unauthenticated length field cannot make it reserve
megabytes.

### Transport encryption

Screen images and input are encrypted, not merely authenticated. This matters
most for input: a key press is a three-byte record, so a passive listener could
otherwise reconstruct a typed password, SSH passphrase or 2FA code byte for
byte with no image analysis at all.

Transport version **3** uses Snow's standard
`Noise_NNpsk2_25519_ChaChaPoly_SHA256` handshake. Both public magic/version/nonce
prefixes, in fixed server/client order, are bound into its prologue. Nonces are
public context, not ephemeral private-key seeds. The library generates fresh
ephemeral keys; handshake payloads are empty and there is no early application
data.

After Noise completes, its standard encrypted transport carries a fixed,
role-separated key envelope containing independently random C2S/S2C record
keys and the handshake hash. Encrypted client and server Finished messages
bind that exact envelope and handshake before either handshake function
succeeds. The existing application record layer then uses those independent
keys, never Noise's internal keys or nonce space. This is **Noise-protected
application key delivery**, not a claim that the subsequent record format is
standard Noise transport.

All handshake flights have fixed small bounds and share the existing absolute
negotiation deadline. Wrong keys, versions, malformed flights or missing
confirmation close the connection. There is no downgrade or v2 fallback.
Upgrade both endpoints together. This transport update originally accompanied
application v23; the released baseline is application v29 and the next candidate is v30 (unified-control repair rerun pending). Transport v3 and the
native Wayland extension versions are separate version domains.

Each record is `kind || sequence || ciphertext_len || ciphertext || tag(16)`.
The plaintext is sealed with ChaCha20-Poly1305 under a nonce of four zero bytes
followed by the record's big-endian sequence, with the 13-byte header
authenticated as associated data. Sequence numbers start at zero, advance by
exactly one, and exhausting the range is a hard error on both the read and the
write path, so a key/nonce pair is never reused. The 16-byte tag is *smaller*
than the 32-byte HMAC it replaces, and the whole record is assembled in one
reusable buffer and written with a single `write_all` — both peers set
`TCP_NODELAY` and neither wraps a buffered writer, so this replaced three
unbuffered socket writes per record with one.

Small records are padded to 64, 256 or 704 bytes before sealing, because length
alone otherwise distinguishes a keystroke batch from a pointer run from a
release-all. Larger records are not padded: a screen frame's size is dominated
by its content and no bucket could hide that. **Inter-keystroke timing remains
a side channel** — padding does not hide when you type, only what you type.

Historical record-layer measurement, not a v3 handshake benchmark: on a
3440x1440 native-resolution session, the largest payloads this carries, the telemetry `write` stage stayed at 0.0-0.1 ms, unchanged from
the unencrypted transport. ChaCha20-Poly1305 is pure Rust here and adds no C
toolchain or system dependency to the helper.

After ephemeral secrets and session keys have been destroyed, later disclosure
of only the long-term PSK should not decrypt previously recorded v3 sessions.
This does not protect sessions whose live endpoints or random source are
compromised, prevent an attacker already holding the PSK from impersonating a
peer, or retroactively protect recorded v2 sessions. Explicit zeroization
reduces retained key material; Rust/runtime copies are not a guarantee of
complete process-memory erasure. In particular, the selected Snow version does
not explicitly zeroize every internal DH/PSK/cipher-state allocation; dropping
its state must not be advertised as protection against later process-memory
inspection. Rotate a compromised PSK and keep using SSH
when you want an independently authenticated channel:

```bash
# Host
jwm-remote host --key-file ~/.config/jwm/remote.key --allow-input

# Client (keep this running), then connect to 127.0.0.1 below
ssh -N -L 48221:127.0.0.1:48221 user@192.168.1.50

# Client (in a second shell)
jwm-remote connect 127.0.0.1:48221 \
  --key-file ~/.config/jwm/remote.key --grab-input
```

## Build

The default build includes `jwm-remote`:

```bash
cargo build --locked --release --bin jwm-remote
```

Both the source installer and managed release bundles expose
`/usr/local/bin/jwm-remote`. The managed path is a relative symlink to
`/usr/local/lib/jwm/current/bin/jwm-remote`; upgrades from older manifests can
add this stable path. An existing source-installed helper is replaced only
with explicit `--replace`, backed up, and restored by final uninstall.
Rolling back to a release without the helper leaves the managed symlink
inactive until a release containing it becomes current again.

Slim X11 builds must keep the `remote-x11` feature. The helper still works
with either selected JWM backend because it opens its own X11 connection:

```bash
cargo build --locked --release --no-default-features \
  --features backend-x11rb,remote-x11 --bin jwm --bin jwm-remote

cargo build --locked --release --no-default-features \
  --features backend-xcb,remote-x11 --bin jwm --bin jwm-remote
```

Both machines need an X11 JWM session. The host X server needs Composite;
interactive control additionally needs XTEST. The two machines should use the
same XKB layout for keyboard control because the MVP forwards X11 keycodes
(physical keys), not text. The handshake compares both keymaps; a mismatch
fails closed by disabling keyboard injection while leaving pointer control on.

### Keyboard layouts

The viewer sends a 3-byte digest of every keycode's base symbols — the first
two levels of the first two groups — and the host compares it key by key with
its own map. A keycode bound on both machines must produce the same symbols.
A keycode bound on only one side is ignored: the viewer's keyboard cannot
produce it, or the host has nothing on it, so it cannot type a wrong
character. Modifier maps must match exactly, and at least 32 keycodes must be
bound on both sides.

The earlier whole-map digest failed on real desktops with identical layouts.
Tools that type text through XTEST bind spare keycodes to the characters they
need — one desktop had 17 unused keycodes bound to CJK characters left over
from an automation — and the core table can widen as a side effect, so the
digests never matched and keyboard control silently fell back to pointer-only.
The per-key comparison tolerates both. Columns past the first four are not
compared, because their meaning depends on the table width; a layout that
differs only in AltGr (third-level) symbols is therefore not detected.

When keys do differ, the host terminal names them:

```text
jwm-remote: peer X11 keymap differs (2 keys differ (keycodes 29, 52)); keyboard disabled, pointer remains enabled
```

A keymap change during a session is checked the same way: binding a spare
keycode keeps the session, remapping a real key ends it.

## First LAN connection

Create the key once on either machine. The command refuses to overwrite an
existing path:

```bash
install -d -m 700 ~/.config/jwm
jwm-remote keygen --output ~/.config/jwm/remote.key
```

Copy that file to the other machine through a secure channel, then restore its
private mode:

```bash
ssh user@192.168.1.60 'install -d -m 700 ~/.config/jwm'
scp ~/.config/jwm/remote.key user@192.168.1.60:~/.config/jwm/remote.key
ssh user@192.168.1.60 'chmod 600 ~/.config/jwm/remote.key'
```

On the machine being controlled:

```bash
jwm-remote host \
  --listen 0.0.0.0:48221 \
  --allow-lan \
  --allow-input \
  --key-file ~/.config/jwm/remote.key
```

Restrict TCP port `48221` to the intended local subnet in the host firewall.
On the controlling JWM machine, replace the address with the host's LAN IP:

```bash
jwm-remote connect 192.168.1.50:48221 \
  --key-file ~/.config/jwm/remote.key \
  --grab-input
```

With `--grab-input`, click inside the viewer before using JWM shortcuts. Press
**Ctrl+Super+F12** (Super is the Win key) to release the local keyboard/pointer
grab and every remotely held key/button. This matches JWM's default shortcut
for third-party remote desktop passthrough. Caps Lock, Num Lock and held mouse
buttons do not prevent release. The client refuses `--grab-input` if its base
keymap has no plain F12 key; `--escape-key` changes the shortcut's base X keysym.
For example, `--escape-key Pause` uses Ctrl+Super+Pause.

Without Ctrl+Super, F12 is forwarded to the remote machine like any other key,
including repeated presses and double taps. The release shortcut itself is
handled locally. Closing the viewer also releases input. Without
`--grab-input`, ordinary pointer and key events are forwarded while the viewer
has focus, but local JWM global shortcuts may win their passive grabs.

For view-only use, omit host `--allow-input` or pass client `--view-only`:

```bash
jwm-remote connect 192.168.1.50:48221 \
  --key-file ~/.config/jwm/remote.key \
  --view-only
```

## Tuning and troubleshooting

The interactive LAN defaults are a 30 FPS upper limit, adaptive JPEG quality
40–85, and `--max-width 0`: fit the actual viewer window up to the source's
native dimensions, with no default 1280-pixel ceiling. A 2560-pixel desktop
previously became 1280 pixels even when the viewer could show more; enlarging
that viewer only magnified the lost detail. Explicit width limits still win,
source dimensions are never upscaled for encoding, and all existing dimension,
pixel-count and payload safety limits remain. The first frame may use native
source dimensions before the viewer reports its size, so very large roots can
cost more at startup; set an explicit ceiling when needed. The viewer window
itself opens at that frame's size shrunk to 90% of the local monitor under the
pointer (aspect kept, never enlarged) and centred on it. A 5120x1440 desk viewed
from a 2560x1600 laptop used to open a 5120-pixel window, mostly off-screen
wherever the window manager did not tile it; it now opens at 2304x648, and the
host narrows the stream to that size from the second frame on.

The quality floor stays 40 so congested sessions can still shed detail. A
higher frame/quality ceiling is not a promise to sustain that rate: two-frame
ACK credit, latest-frame replacement and dirty-tile suppression still bound
work and backlog. For a constrained link or host, opt back into the previous
lower-cost settings:

```bash
jwm-remote host ... --fps 12 --jpeg-quality 70 --max-width 1280
```

Other explicit ceilings remain available:

```bash
jwm-remote host ... --fps 24 --jpeg-quality 80 --max-width 1920
```

`--jpeg-quality` is the adaptive upper bound. Repeated same-setting pressure
from display-ACK latency, viewer superseding or exhausted two-frame credit
causes a multiplicative decrease; isolated slow ACKs are absorbed by a small
hysteresis score. Recovery is additive (`+1`) only after at least three seconds
and an FPS-scaled run of healthy ACKs. `--jpeg-quality-floor` changes the
default floor of 40; when the upper bound is below 40, the implicit floor
follows that upper bound for backwards compatibility. Use
`--fixed-jpeg-quality` to disable adaptation and always use the requested
quality (it conflicts with an explicit floor). Encoder quality changes do not
change the remote wire protocol.

`--max-width 0` permits native width but still fits the negotiated viewer width
and height; it does not force a native-size stream into a smaller window.
Higher resolution, quality, and frame rate increase CPU and bandwidth together. With XRender 0.10+,
`--max-width` also limits the X11 readback size for both overlay and root
capture. Root mode uses an `IncludeInferiors` graphics context to copy the root
and its same-depth children into a full-size staging pixmap, and creates a
Picture only for that pixmap, never for the root Window. It then scales into the
small target used by MIT-SHM or core readback. Core X11 does not guarantee the
result for inferiors at a different depth; a rejected request uses the CPU
path. The staging pixmap is limited to 64 MiB,
including the peak during geometry replacement; an oversized root uses the
existing full-size readback plus CPU resize. The accelerated path prints its
active source/output dimensions. MIT-SHM 1.2 file-descriptor
segments avoid copying image payloads through the X11 socket when the local X
server and transport support them; setup or runtime failure falls back to core
`GetImage` on the same frame and drawable. Standard depth-24, 32-bpp
little-endian TrueColor images are converted directly from native BGRX rows to
owned RGB storage; the host validates the current visual masks, byte order and
row layout before using that path, while all other formats keep the generic
pixel decoder. Capture and JPEG/network sending run as a two-stage pipeline
with one queued latest frame. The host permits at most two frames beyond the
latest one actually drawn by the viewer; while that credit is exhausted,
redundant X11 readback is automatically reduced to a periodic 250–1000 ms
refresh. An empty queue resumes the requested rate on its next tick.

The host caches the root geometry, compositor owner and XFixes cursor shape.
Checked core/RandR and XFixes notifications invalidate those caches, after
which the host performs one authoritative `GetGeometry`, `GetSelectionOwner` or
`GetCursorImage` query. A stable frame therefore fetches only the lightweight
pointer position and reuses the premultiplied cursor pixels, including their
scaled form. A cursor serial change during readback suppresses the stale cursor
for that frame and refreshes it on the next frame. Every source mode tracks
compositor-manager owner changes so a newly started compositor receives a fresh
capture-inhibitor property notification; event-driven stable frames do not poll
that selection, and Root mode never acquires the overlay. If an individual
notification facility is unavailable, only that cache returns to its reliable
polling path. RandR resize or compositor-owner races are reconciled and retried
once before XRender is disabled or Auto capture falls back to root; a transient
overlay-acquire failure is retried with a bounded delay. A staged root capture
also records the geometry epoch before `CopyArea` and validates it after the
small readback drains notifications. One changed epoch retries from an
authoritative geometry; repeated churn uses CPU for that frame. Polling mode
performs the same post-readback check with `GetGeometry`.

When the Composite overlay and XDamage are available, the host also avoids a
full drawable readback on a completely unchanged tick. It captures the first
frame and then whenever the active overlay reports Damage, the compositor owner
or root geometry changes, the composited cursor shape or pointer position changes, or a
two-second forced-refresh deadline expires. A Damage subtract request is queued
only after a tick commits to capture and before the synchronous drawable
readback on the same connection. That reply is the ordering barrier; an
asynchronous Damage rejection is drained before publication and permanently
disables Damage gating. A notification arriving during readback remains dirty
for the next tick, and failed readback never advances the deadline. The cursor
baseline records the position actually composited into the successful frame,
not an earlier gate probe. Root capture never negotiates XDamage. If the
extension is absent or a Damage request is rejected, overlay capture
permanently returns to the existing per-tick path for that session; X11
transport failures still end the broken session instead of pretending to fall
back.

The host emits rolling pipeline telemetry every five seconds, including an
explicit zero-send window while the sender is blocked on display credit or a
socket write. The viewer emits each non-empty five-second activity window. Both
peers emit one final non-empty partial snapshot during cleanup. Host counters
cover scheduled, captured, skipped, `damage-skipped`, published, dequeued,
encoded and sent work,
payload bytes, current/maximum outstanding credit and maximum queue age; its
stage averages cover capture, queue, credit wait, encode and write time.
`skipped` is capture-mailbox backpressure, while `damage-skipped` means XDamage,
geometry and cursor state proved that no readback was needed; those ticks do not
re-submit an old image to unchanged-frame comparison. Host
`replaced` means that a newer captured frame overwrote the one-slot
capture-to-sender mailbox before encoding. Viewer `replaced` instead means that
a newer decoded frame overwrote the viewer's one-slot latest-frame queue before
drawing; the viewer also reports received, decoded, drawn and ACKed counts plus
separate decode, queue and draw time.

A cumulative ACK for sequence B proves only that B was drawn. The host therefore
reports one `drawn-acks` target, all credits through B as `retired`, and the
earlier retired targets without an individual ACK as `viewer-superseded`.
`capture-to-ACK` and `send-to-ACK` end when the host receives that ACK, so they
include viewer queue/draw work, ACK creation and flush, and return-network delay;
they are not the viewer's draw-call duration. `drawn-bytes` counts only the JPEG
payloads of those proven ACK targets. `credit-wait` separately measures how long
the sender waited for an available display credit.

Adaptive quality attaches the exact encoder quality and a local setting epoch
to each in-flight credit. Only ACKs for the current epoch affect its pressure
score, and cumulative superseding counts only earlier frames from that same
epoch; late ACKs from the previous quality therefore cannot cause a second
decrease. Payload size remains telemetry and diagnostic context rather than a
discrete quality threshold, avoiding oscillation when JPEG output crosses an
arbitrary byte boundary. The ACK thread queues only bounded scalar feedback;
the video sender alone updates and reads quality immediately before encoding,
with encoding, logging and frame destruction outside the controller lock.

### Dirty-tile delta coding

Video is coded as 16-pixel tiles rather than whole frames. Before requesting a
quality setting or encoding, the sender compares the dequeued capture against
the pixels the viewer was last *sent* and collects the tiles that differ by
more than a small per-channel tolerance. Only those tiles are copied into one
packed atlas image, and that single atlas is JPEG-encoded.

The reference is deliberately the last transmitted content rather than the
previous capture. A region drifting slowly therefore still crosses the
tolerance against its own stale copy and is retransmitted, so the tolerance
bounds the error instead of letting it accumulate. It also makes committing a
frame proportional to the dirty area rather than the whole image.

One atlas beats one JPEG per dirty rectangle by roughly 3x on real desktops,
because a few hundred small rectangles otherwise pay a few hundred JPEG headers
and lose all shared Huffman statistics. Sixteen-pixel tiles keep every tile
aligned to a 4:2:0 minimum coded unit, so atlas neighbours cannot bleed chroma
into each other.

A frame with no dirty tiles is not sent at all, which subsumes the previous
exact-duplicate suppressor. At the four-second keepalive boundary an empty tile
frame — a header and a bitmap, no atlas — is sent instead, keeping static
sessions inside the viewer's shared eight-second video idle timeout. The
conservative timing budget is four seconds of keepalive plus the two-second
forced XDamage refresh and one second of scheduling margin, still strictly
below that eight-second timeout.

The first frame of a session, a root geometry change and an encoded-size change
each force a keyframe carrying every tile. A failed write or flush never
advances the reference, so the host's model of the viewer's canvas cannot run
ahead of what the viewer actually received.

Measured on a 3440x1440 JWM session sharing a lightly-active desktop over
loopback, against the previous whole-frame encoder:

| | whole-frame | dirty-tile | |
|---|---|---|---|
| wire, `--max-width 1280` | 10.1 Mbit/s | 0.5-0.9 Mbit/s | ~15x |
| wire, `--max-width 0` | 53.3 Mbit/s | 1.8-2.6 Mbit/s | ~25x |
| encode, 1280 wide | 9-17 ms | 0.4-1.2 ms | ~20x |
| encode, native | 60 ms | 1.5-2.7 ms | ~30x |
| capture-to-ACK, native | 80-89 ms | 12-14 ms | ~6x |

The 60 ms native encode had capped the achievable rate near 16 fps regardless
of link speed; that ceiling is gone.

Host telemetry reports suppressed frames as `unchanged-suppressed` and periodic
empty frames as `unchanged-keepalive`; both are subsets of `dequeued`, while
only the latter is also `encoded` and `sent`. `keyframes` counts frames
carrying every tile, and `tiles A/B (P% dirty)` reports transmitted versus
total tiles for the window — the single best indicator of how much the codec is
actually saving on the content being shared.

The sender writes each JPEG directly behind its frame header in one reusable
allocation while hard-bounding the total payload length, and the viewer's
receiver likewise reuses one authenticated record allocation. After 32
substantially smaller payloads, an allocation retained by an exceptional frame
shrinks toward an 8 MiB ceiling; normal steady frame sizes do not churn
allocations.
After authentication and header validation, an ordinary RGB JPEG is decoded
directly into a reusable RGB allocation. The allocation stays leased while the
frame is queued or retained by the X11 viewer for Expose and resize, then
returns only after replacement, draw failure, or window close; two concurrently
live frames therefore never alias. The best-fit free list keeps at most two
buffers, rejects any slot above 32 MiB, and retains at most 64 MiB total.
Uncommon grayscale JPEGs retain the compatible bounded conversion path.
The X11 viewer reuses its native upload allocation for unchanged display
geometry. On the usual depth-24, 32-bits-per-pixel little-endian TrueColor
visual it writes decoded RGB directly into the verified native layout and uses
an MIT-SHM 1.2 file-descriptor segment to avoid sending that image through the
X11 socket. The segment is reused only after the matching server completion
event; setup or rejected-upload failures retry the same frame with core
`PutImage`. Nonstandard visuals use the general mask-based conversion and core
upload. Expose events copy the last completely uploaded backing image, and
resize event bursts redraw only their final size.
When a large viewer window displays the encoded image at more than its native
width and height, XRender 0.10+ keeps the uploaded image in a source-sized
server pixmap and scales it into the letterboxed backing pixmap. The client no
longer constructs or uploads a window-sized image on every new frame, so a
bandwidth-friendly host `--max-width` can still be viewed fullscreen without
moving the corresponding upscale work through CPU memory. This changes only
local presentation: it does not increase the encoded resolution or network
traffic. One-to-one presentation and downscaling keep the existing CPU path;
an unavailable or unusable XRender path falls back there as well. On NVIDIA's
proprietary driver, XRender composite into backing pixmaps similarly suffers
from asynchronous pipeline races with core CopyArea; under `--scaling auto`
(the default), the viewer automatically upscales on the CPU to prevent blocky
black-and-white noise jitter. `--scaling gpu` overrides this.
When idle, the viewer blocks on its X11 connection, the video receiver's wake
descriptor, and the nearest heartbeat, telemetry or deferred-key deadline; it
also checks x11rb's internal and MIT-SHM-deferred event queues before sleeping.
With input negotiated, closing the viewer flushes the close-time `ReleaseAll`
batch before sending one authenticated `Close`; view-only close sends only the
`Close`. If X11 drawing or network I/O has already failed, the client instead
shuts the session transport down immediately and lets the host's disconnect
cleanup release injected input. Cancellation and shutdown are idempotent, and
the blocking receiver thread gets a bounded join window.
Root capture queues its `IncludeInferiors` `CopyArea` and XRender Composite
before the synchronous small-target readback, then checks both requests against
that ordering barrier. A rejected request cannot publish an older target image.
Older or unusable XRender servers, staging roots above 64 MiB, and per-frame
staging failures retain the full-resolution readback plus CPU-resize fallback;
for those paths, lower `--fps` as well as `--max-width` on very large combined
multi-monitor roots.
Every frame write has a 10-second absolute deadline across its header, JPEG,
authenticator, and flush. If it expires, the host tears down that session and
the normal cleanup path releases any keys or buttons held by its controller.

### A second viewer

Only one controller at a time. A second viewer used to complete its TCP
connect into the listen backlog and then hear nothing until its handshake
timeout expired, reporting a bare end-of-file — indistinguishable from a wrong
address, a firewall, or a host that had died. It is now authenticated, told
`another viewer is already connected to this host`, and disconnected in
milliseconds.

Authentication comes first deliberately: only a peer holding the key learns
that a session is in progress. Every connection is authenticated on its own
thread under the five-second handshake deadline, and the single viewer slot is
claimed only after a peer has completed the handshake and sent its `Hello`.
A connection that says nothing therefore cannot make the host look busy to the
real viewer, cannot stall the connections behind it, and cannot learn from
the timing of its greeting whether a session is running. At most eight
unauthenticated handshakes run at once; further connections are closed
immediately. The active session runs on its own thread, so refusing a second
peer never interrupts the first.

### Session resilience

The listener survives everything that concerns a single connection. An
unauthenticated peer that connects and immediately resets, an aborted
connection, an interrupted syscall, or descriptor exhaustion are all logged and
retried rather than ending the host; only a genuinely broken listener stops it.
The peer address comes from `accept` itself, because asking the accepted socket
for it is a liveness question that a reset peer answers with an error.

`--once` returns the first authenticated session's own result, so a scripted
run that failed is distinguishable from a clean one by exit code. Port scans
and peers without the key do not count as that session.

While the controller holds any key or button, the host requires it to stay
audible: 600 ms of silence releases everything held, without ending the
session. The host X server generates autorepeat, so without this a network
partition with a key down kept typing into whatever had focus for the full
eight-second idle timeout. The host waits for the socket to become readable
rather than shortening its read timeout, so a record is never interrupted
part-way through.

A button the peer sends that this host's pointer map does not define — a
12-button mouse, or horizontal-scroll buttons 6 and 7 — is dropped rather than
failing the batch. Because a batch is validated before anything is queued,
failing it also discarded every pointer motion and any release-all travelling
alongside. The physical button is pinned when a press is queued, so a pointer
remap arriving mid-press cannot release a different button and leave the real
one stuck down.

The client bounds its TCP connect. Every later phase already had an absolute
deadline, but a black-holed address otherwise burned the kernel's full SYN
retry budget against a five-second negotiation budget.

An overlay readback failure now arms the same bounded retry as an overlay
acquire failure. Without it, re-acquisition waited for a compositor-owner
transition that never arrives while the same compositor keeps running, so one
transient error downgraded the session to ungated root capture — and with it
the XDamage gate — for the rest of the session.

A failure inside a scaled capture is attributed to the stage that actually
failed. The accelerated path issues `CopyArea` and `Composite` and then reads
the small target back, and a failure of that readback used to be reported as an
XRender fault. Since losing XRender means every later frame reads back the
full-resolution drawable and resizes it on the CPU, one transient readback
error bought roughly a hundredfold host-CPU increase for the rest of the
session. Readback failures now take the ordinary readback path, and a genuine
XRender rejection suspends scaling with a 1 s / 5 s / 30 s backoff, retiring it
only after four consecutive failures.

An unparsed X11 event is attributed to the extension that owns its event code
and demotes only that facility: XDamage for the damage gate, XFixes for the
cursor and compositor-owner caches, RandR for the root geometry. Previously any
single unrecognized event retired all four at once with no path back, taking
steady state from one blocking round trip per frame to about five. An event
belonging to no extension this connection selected cannot have cost a
notification it depends on, so those are tolerated in a run of eight before the
session distrusts its caches wholesale.

### Switching among explicitly granted sources

Application protocol v11 adds a host-authored source catalog and in-session
source selection. Upgrade both helpers together; v10 and earlier peers are
rejected, with no legacy fallback. This does not change transport encryption.

The default catalog contains **only** the initial `--monitor`, `--region`, or
root selection. Selecting a monitor or rectangle does not also grant the root.
Add grants explicitly when starting the host:

```sh
jwm-remote host --monitor DP-0 --share-monitor DP-1 --share-region 800x600+0+0
jwm-remote ssh user@desktop --monitor DP-0 --share-monitor DP-1 --view-only
# Include the whole desktop only when that is intended:
jwm-remote host --monitor DP-0 --share-root
```

`--share-monitor NAME` and `--share-region WxH+X+Y` are repeatable; `--share-root`
is explicit whole-root permission. Monitor names are bounded to 256 bytes and
cannot contain control characters. At most 31 extra flag values are accepted,
with at most 32 unique sources including the initial selection. Exact duplicate
grants are collapsed. IDs are deterministic within a session: the initial source
is 1, followed by unique monitors, regions, then root. Names and rectangles are
host configuration, never accepted as arbitrary viewer requests. Every grant
is validated at startup; a missing source is an error, never a root fallback.

In the viewer, **Ctrl+Super+Shift+F12** cycles the granted sources, including in
view-only mode. With `--escape-key Pause`, use Ctrl+Super+Shift+Pause instead.
The shortcut stays local, releases input grabs, and holds input until the new
source's frame arrives. Ordinary Ctrl+Super+F12 still releases control.

Grants are immutable for the session. Switching selects only an existing ID;
it cannot broaden sharing, clipboard permission, or input permission. Source
changes reset frame/cursor state and release held remote input so old coordinates
cannot act on the new picture. Invalid requests, unavailable sources, and unsafe
geometry transitions close the session rather than silently changing scope.
Changing the source list requires starting a new host session. Existing per-monitor
SSH launch shortcuts remain a separate close-and-reopen workflow. A changed
extra-grant list gets a distinct session identity, so SSH does not silently
reuse a running session with a different grant list.

### Sharing one monitor

The X root spans every monitor, so a dual 1920x1080 desk is one 3840x1080
drawable; an explicit low-bandwidth `--max-width 1280` delivers each display at 640x360.
On a triple head it is 426x240. Sharing one monitor is what makes that legible
again:

```bash
jwm-remote host ... --monitor DP-1
jwm-remote host ... --region 1920x1080+1920+0
```

`--monitor` takes a RandR monitor name — `xrandr --listmonitors` lists them —
and is re-resolved by name whenever the layout changes. A name that does not
exist is an error at startup, listing the names that do, rather than a failure
discovered when someone finally connects. `--region` takes a fixed rectangle
in root coordinates using the usual X11 `WxH+X+Y` spelling and accepts the
negative offsets a left-hand monitor has.

Either way the shared area is clipped to the root. During an existing
connection, a change to the resolved area's position or dimensions ends the
connection rather than silently reinterpreting the frame and input mapping.
This also applies when root clipping changes a fixed `--region`. Reconnect
after verifying the selected area; layout changes do not move the shared area in place. Explicit selection
of another already granted source uses the v11 switch flow above. A missing selected monitor or an area entirely outside
the root is an error rather than a black rectangle.

Input follows the picture. The viewer maps its window onto the shared area, so
the coordinates it sends are area-relative while XTEST warps in root
coordinates; the host adds the origin before injecting. Without that, sharing
the right-hand monitor would land every click on the left one. The host cursor
is translated the same way and clipped when it leaves the shared area.

Sharing a region also shrinks the root-capture staging pixmap to the region
rather than the whole root, which is what the 64 MiB staging limit is measured
against.

### Clipboard sharing

Off unless **both** sides ask for it: the host needs `--allow-clipboard` and
the viewer needs `--clipboard` (`jwm-remote ssh ... --clipboard` sets both).
The host's flag is policy and the client's is consent; if either is missing
the capability is simply absent from the negotiated session and no clipboard
record is ever sent.

```bash
jwm-remote host ... --allow-clipboard
jwm-remote connect ... --clipboard
```

**Narrowing what is shared.** `host`, `connect` and `ssh` accept:

- `--clipboard-mode text|text-images|all` (default `all`).
- `--clipboard-direction both|to-host|from-host` (default `both`). Directions
  always name the viewer and host, even on the host command line.

These flags require the existing explicit consent flag (`--allow-clipboard`
on host, `--clipboard` on connect/SSH); they never enable sharing alone. Each
end may narrow policy. The effective policy is their intersection, never a
broader grant. `--clipboard-file-limit 0` still disables files. For example:

```sh
jwm-remote host --allow-clipboard --clipboard-mode text --clipboard-direction to-host
jwm-remote connect HOST --clipboard --clipboard-mode text --clipboard-direction to-host
jwm-remote ssh user@desktop --clipboard --clipboard-mode text-images --clipboard-direction from-host
```

SSH session reuse includes clipboard consent, effective content/direction and
file limit, input permission, source grants and frame limits. A different
immutable policy is never applied to or used to focus a broader existing
session. Close that session explicitly, then start the new one; a policy change
does not silently close it. Switching monitors with identical policy retains
the existing monitor-switch behavior. Local window size does not change identity.

Text-only does not request image/file payloads or read copied file contents.
A file manager may still offer its paths as ordinary text, which text-only can
share. Receive-only endpoints do not capture local clipboard contents. Initial
direction restrictions stay fixed for the session. Saved profiles preserve
initial narrowing, and missing scope fields retain legacy all/both semantics
only when the required clipboard consent boolean is already true.

**Restrict sharing without reconnecting.** In the viewer, press
**Ctrl+Super+Alt+F12** to narrow content one step: all → text + images → text →
off. With `--escape-key Pause`, use Ctrl+Super+Alt+Pause. This works in view-only
mode and releases remotely held modifiers. Ctrl+Super alone still releases
input; Ctrl+Super+Shift still cycles sources. The shortcut does nothing if
clipboard sharing was not negotiated or is already off. It cannot enable
sharing, restore a removed content type, change direction permissions or modify
a saved profile. Start a new session explicitly to broaden sharing.

The title and terminal distinguish a pending restriction from confirmed scope.
Local admission narrows immediately, but confirmation waits for both endpoints'
transfer workers, backend clipboard fences and ordered outgoing records. Only
one restriction can be pending; repeat presses do not issue overlapping
requests. Failure to confirm within the bounded timeout ends the session without
restoring permissions. Native synchronous X11 or file operations can delay this
confirmation. Newly forbidden transfers are cancelled and unfulfilled offers
abandoned; cumulative transfer accounting remains intact.

Restriction cannot retract bytes already transmitted, files already advertised,
clipboard-manager history, or a paste already underway. Already advertised files
retain the existing staging-retention policy. This is a forward sharing boundary,
not an erasure guarantee. Shortcut, clipboard-owner and live paste behavior still
require isolated live-X11 verification; deterministic tests do not establish it.

With the default all/both settings, whatever you copy on either desktop is what
you paste on the other:

- **Text** in any application, as UTF-8, up to 256 KiB.
- **Images** — a screenshot, an image copied in a browser or an image
  editor — as PNG, up to 32 MiB. JPEG and BMP offers are converted to PNG.
- **Files and folders** copied in a file manager (Nautilus, Thunar, PCManFM,
  Nemo, Caja, Dolphin: anything offering `x-special/gnome-copied-files` or a
  `text/uri-list` of local files). Paste them in the other machine's file
  manager. A terminal or editor pastes their paths there instead.

Each side watches CLIPBOARD on **its own X connection and thread**, and the
copy itself is moved by a separate worker: a selection conversion blocks until
the current owner answers, reading files can be slow, and neither may delay a
frame or an input event.

**How files arrive.** A copy is sent as soon as it is made, not when it is
pasted. The receiving machine takes CLIPBOARD at once, so it changes the moment
the copy was made; if you paste before the last byte has arrived, the paste
waits for it (up to 25 seconds, then it is refused and works once the copy has
landed). The viewer's title shows `receiving 3 files 40%`, then `3 files ready
to paste`; both terminals print a line when a copy starts and lands. Received
files are written under
`$XDG_CACHE_HOME/jwm/remote-clipboard/` (by default `~/.cache/...`), on disk
rather than in the RAM-backed runtime directory, in a private directory per
session. Pasting copies them out of there. The three most recent copies are
kept during a session and the newest one after it, so a paste after
disconnecting still works; the next session removes what a finished session
left behind.

**The newest copy wins**, on both machines. Copying anything while a copy is
still travelling, in either direction, cancels that transfer. A cut is shared
as a copy: the originals are never deleted on the other machine's behalf.

**What is carried.** File contents, names (raw bytes, so any name survives),
folders, modification times and executable bits. Not carried: symlinks inside a
copied folder (a copied symlink itself is followed), devices, sockets and
FIFOs, owners, and set-id bits. Unreadable files are left out and counted in
the terminal. The receiver rebuilds the tree inside a fresh private directory:
every name component is checked, nothing is created through a symlink, and
every file and folder must be new, so a peer cannot write outside it.

**Size.** `--clipboard-file-limit SIZE` (default `2G`, on `host`, `connect`
and `ssh`) bounds one file copy; each end enforces its own limit, as sender
and as receiver, and checks it before reading a byte. `0` shares text and
images only. A receiver also refuses a copy that would leave less than
256 MiB free on its disk. A copy over the limit is not shared and the title
says `copy not shared: too large`.

**Flow control.** Images and files travel in 128 KiB chunks. The receiver
acknowledges a chunk only once it is written, and the sender keeps at most
1 MiB unacknowledged across the whole session, so a slow disk throttles the
sender, and a frame or an input batch never waits behind more than a megabyte
of copied data. A peer that overruns that window ends the session.

**On connect,** if its outgoing direction is enabled, the viewer shares what is already on its clipboard — what you
copied just before connecting is usually what you mean to paste — and the host
does not, so the two machines never swap clipboards.

Two limits from JWM's local clipboard history apply here too. Text marked
secret by a password manager — the `x-kde-passwordManagerHint` convention and
its variants — is never shared. And payloads are validated before they are
handed to the local X server: text must be valid UTF-8 within its cap, and an
image must decode as a PNG within 16 megapixels.

**Understand what this shares.** With default all/both clipboard sharing on, everything you copy
on either machine crosses the network to the other, for as long as the session
lasts — including files you copy in a file manager, which are sent whether or
not you paste them there. That is the point of the feature, but it is a much
broader grant than screen sharing alone, so it stays opt-in on both ends rather
than following from `--allow-input`. Use `--clipboard-file-limit 0` to share
text and images but not files.

#### Optional local clipboard staging cap

The per-copy limit above does not bound all received copies across sessions.
To opt into a shared admission cap, run these commands **on each receiving
machine separately**, as the same user and with the same cache environment as
its `jwm-remote` processes:

```sh
jwm-remote clipboard-staging status
jwm-remote clipboard-staging set-limit 8G  # Example only; choose your own limit.
jwm-remote clipboard-staging disable
```

There is **no numeric default**: staging remains uncapped unless you explicitly
run `set-limit`. The setting is local to the private
`$XDG_CACHE_HOME/jwm/remote-clipboard/` root (otherwise
`$HOME/.cache/jwm/remote-clipboard/`). It is not forwarded by `ssh`, negotiated
with the peer, or stored/overridden by a connection profile. A different cache
root has independent settings and accounting. These maintenance commands open
neither X11 nor a network connection and do not grant clipboard consent.

`SIZE` accepts positive whole bytes or binary K/M/G/T suffixes, including
KB/KiB spellings, case-insensitively. Surrounding whitespace and a space before
the suffix are accepted. Exact fractions such as `1.5G` are accepted; fractional
bytes, zero, overflow and scientific notation are rejected. Fractions have at
most 18 decimal places. Use `disable`, not a zero limit, to return to uncapped
admission. The existing `--clipboard-file-limit` parser and defaults are unchanged.

- `status` is read-only: it does not create a missing cache root, write or repair
  accounting, reclaim stale records, or remove files. It prints the escaped
  local root, cap in exact bytes (or `disabled (uncapped)`), reserved archive
  bytes, reservation count and whether accounting has been initialized. Safe
  inactive legacy copies are included through a bounded read-only inventory
  when needed. An initialized accounting file does not mean a cap is enabled.
  Unsafe or uncertain inventory is an error, not an assumed zero.
- `set-limit` explicitly enables or replaces the cap. A requested limit below
  current accounted usage fails without replacing the previous setting or
  evicting copies. Choose a sufficient limit or resolve the existing retained
  files through their normal lifecycle first. Setting a cap does not forcibly
  cancel already admitted transfers.
- `disable` removes the cap while preserving accounting and staged files. It
  does not reset usage, delete copies, or promise that the disk has free space.
  If complete accounting cannot be inspected, it fails without reporting a
  partial total.

**Upgrading an existing cache.** Upgraded receivers track their reservations
persistently even before a cap is enabled. Until the first successful
`set-limit`, they can coexist with untracked live older processes, whose
session directories are not swept while those processes may still be running.
Because those old writers do not report reservations, `status` and first
activation refuse to report a complete total while one is live. Stop older
`jwm-remote` processes that used this root before enabling the cap. A live
upgraded process with tracked reservations does not by itself prevent activation.

First activation imports safe inactive legacy copies without removing them,
checks that the requested cap covers all accounted usage, and permanently
requires tracked writers for that root. **Disabling the cap does not undo that
tracking requirement.** Use upgraded binaries thereafter: old binaries can
bypass the cap, and unexpected untracked directories make newer operations
refuse rather than guess. Incomplete, malformed or identity-mismatched metadata
is refused rather than silently reset. Unknown process state and PID reuse
are treated conservatively, so old files or reservations may remain charged.

The cap charges **logical reserved archive bytes**, including serialized entry
headers, names and file contents. It is shared across cooperating processes
using this one private root. An incoming archive must fit in the remaining
budget before its transfer is admitted; it does not grow unaccounted while
being written. Active and retained reservations both consume that budget.
This is not a filesystem quota, disk-block or metadata/inode limit, a global
per-user limit across different roots, or protection against older binaries or
other same-user writers. The existing per-copy limit and free-disk check still
apply independently.

Admission does not evict an older copy to make room. The separate retention
policy remains newest three during a session and newest one after shutdown,
with the existing conservative treatment of advertised or uncertain clipboard
paths. Deletion failures and uncertain cleanup can leave reservations charged;
turning the cap off does not discard that accounting. A cap refusal means the
incoming copy was not admitted, not that an earlier paste has been revoked.

State changes and reservations use a shared root lock. Contention fails
immediately rather than waiting for another process. Metadata and inventory
work have explicit size/count bounds: 4 MiB metadata, 1,024 recorded sessions,
16,384 reservations and 1,088 root entries. Payload-tree walks are capped at
1,000,000 entries, depth 64 and 4,096 relative-path bytes. These bookkeeping
limits still apply when the byte cap is disabled. Filesystem calls themselves
are synchronous and have no elapsed-time bound. A read-only status failure is not
permission to delete accounting files or repair permissions manually. Stop
conflicting processes or investigate the reported filesystem problem before
retrying. Offline tests cover the local accounting and CLI contracts, not live
X11 pasting, real disk-pressure behavior, or a multi-hour resource soak.

### Independent cursor plane

When negotiated, cursor movement and shape changes travel outside video in
bounded authenticated records. `CursorShape` (kind 15) contains shape ID `u64`,
width/height/hotspot coordinates as four `u16`s, then premultiplied RGBA bytes.
Shapes are at most 256x256, with an in-bounds hotspot; IDs must be nonzero.
`CursorPosition` (kind 16) contains sequence, geometry epoch and shape ID as
three `u64`s, source width/height as two `u16`s, signed `i32` x/y, and a strict
0/1 visibility byte. All integers are big-endian. Shape precedes the first
position that references it. Unknown shape references, replayed positions,
backwards epochs and malformed payloads close the session. Motion uses a
single latest-value slot rather than an unbounded queue.

The tile-frame header appends geometry epoch `u64` after the dirty-tile count,
before its bitmap (32-byte header total). A source/bounds epoch change forces
a keyframe even if dimensions and pixels are identical. Decoder snapshots,
viewer RGB reuse and cursor placement cannot cross epochs. A future cursor is
retained but hidden until matching video geometry arrives; old cursor state
cannot overlay a new same-size source. Source monitor changes that previously
required reconnect still require reconnect; epochs do not expand input scope.

The host polls authoritative XFixes shape and pointer position on a separate
16 ms deadline before video-FPS pacing and credit checks. Video readback can
still delay a cursor poll; this is not a separate capture connection/thread.
Cursor messages do not renew the receiver’s video idle budget. Cursor-only
changes no longer invalidate video Damage gating or composite pixels into
captured frames. The viewer caches an ARGB32 XRender Picture and queues OVER
onto its window
after restoring the clean backing. Same-shape motion performs no GetImage,
full-frame upload or synchronous reply wait. Shape changes perform a bounded
upload. The old cursor rectangle is restored and Expose/resize repaints it.
Host-local motion and view-only sessions use the same path.
An oversized/unavailable cursor or recoverable XFixes failure sends a bounded
transparent hidden sentinel, removing an earlier visible cursor safely. A cursor
outside the selected sharing region also uses that sentinel and coordinates
(0,0); private off-region cursor shapes, serials and positions are not sent.
Composited fallback keeps the previous cursor-in-video behavior.
Multi-X-screen servers explicitly negotiate composited fallback: XFixes cursor
images have no root identity, so a pointer crossing roots between replies
cannot be safely admitted. Ordinary multiple monitors on one RandR root
remain supported by the independent plane.

Live X11 behavior and end-to-end latency still require validation on a real
server: this cloud environment cannot create the Unix socket Xvfb needs.

### Viewport negotiation

Before negotiation the viewer queries the monitor under the pointer (falling
back to the primary monitor or root dimensions) and includes its bounds in the
authenticated hello. The host applies these bounds before its first capture,
subject to the same operator ceiling and minimum dimensions as later updates.
A missing initial viewport requests host defaults; a half-empty viewport is
invalid. Window-manager resizing and monitor changes after negotiation are
handled by the normal viewport updates.

The viewer then reports its actual window size, and the host stops encoding pixels that
window cannot show. A 640-wide viewer previously received a 1280-wide image and
threw half of it away on arrival; the request is re-sent whenever the window
resizes, coalesced so an interactive drag produces one request rather than one
per intermediate size.

`--max-width` is a ceiling, not a target. A request may only ever narrow it, so
a peer can never make the host spend more readback, encode time or bandwidth
than the operator allowed; requests below the 320x200 floor are clamped to it.
To let a large viewer receive more detail, raise `--max-width` on the host —
that stays an operator decision.

The encode is fitted inside *both* the viewer's width and its height, whichever
binds harder, preserving aspect and never upscaling. Clamping width alone made
one `--max-width` mean very different amounts of work depending on how the
host's monitors are arranged: a 3440x1440 side-by-side root becomes 1280x535,
while a 2560x2880 stacked root becomes 1280x1440 — 2.7 times the pixels for the
same flag — and a portrait root is barely clamped at all. `--max-width` still
has no height counterpart of its own; the viewer's height is what bounds that
axis.

Changing the encoded size retargets the XRender scaler and forces one keyframe,
since the viewer's canvas geometry has changed. Measured on a 3440x1440 host as
a viewer resized from 1280 to 640 wide: capture fell from 18.3 ms to 9.5 ms per
frame, encode from 2.2 ms to 0.6 ms, and capture-to-ACK from 4.9 ms to 2.4 ms.

### Capture scheduling

`--fps` is a rate limiter, not a schedule. The capture loop waits on the X
connection and captures as soon as the server reports something, never more
often than the requested interval. Capturing on a fixed grid instead made every
interaction wait for the next grid point — a mean of half an interval, 42 ms at
the earlier 12 FPS setting — for a tick that had nothing to do with it.

The wait consumes only the remaining rate-limit interval. Once capture is due,
it does not wait a second complete interval for an X11 event: cursor movement
alone need not produce a selected Damage event. The older double wait could
halve cursor-only update cadence. Early events still cannot exceed `--fps`,
and mailbox/ACK backpressure remains unchanged. Shutdown is checked between
wait slices no longer than 20 ms. Notifications x11rb
already buffered while waiting for a capture's own reply are drained before the
descriptor is polled, so an event in hand never waits on a socket with nothing
left to deliver. The damage gate still decides whether a wake is worth a
readback, including its forced refresh and cursor probing, and an early wake
still passes through capture-mailbox backpressure.

Measured on a live 3440x1440 session at the earlier 12 FPS setting, `scheduled` fell
from a constant 60 per five-second window to 21-51, tracking real screen
activity instead of the clock. A change arriving while the rate limiter is
still holding is not lost: it stays pending in the gate and is captured as soon
as the interval allows.

### Reading the capture mode

Each telemetry line begins with the paths the capture loop is actually taking,
and every transition is announced as it happens:

```text
jwm-remote: host 5.0s mode overlay/xrender/shm/damage/cursor-events scheduled 60 ...
jwm-remote: capture mode now overlay/cpu-resize/shm/damage/cursor-events
```

The healthy reading is `overlay/xrender/shm/damage/cursor-events`, or
`overlay/cpu-resize/core-getimage/damage/cursor-events` on NVIDIA's proprietary driver
(see [Half-drawn frames on NVIDIA](#half-drawn-frames-on-nvidia)). Each field
names its fallback explicitly rather than disappearing, so a degraded session
is obvious at a glance: `root`, `cpu-resize`, `core-getimage`, `no-damage`,
`cursor-poll`, and the `geometry-poll` / `compositor-poll` fields that appear
only when those caches have stopped following notifications. `cpu-resize` and
`no-damage` in particular are large, and each of them used to be announced only
once, by a line that had long since scrolled away.

### Machine-readable pipeline timings

For repeatable terminal/browser/Isaac Sim workloads and physical latency
measurement, see [the workload baseline protocol](remote-workload-benchmark.md).
Offline telemetry analysis does not certify physical input-to-display latency.

`jwm-remote host --stats-json` replaces the five-second human statistics line
with a JSON object on stderr (`type: "jwm_remote_host_stats"`, schema version 1).
Other diagnostics still use stderr, so select JSON objects by their `type`
rather than treating every stderr line as JSON. The final partial window is
also emitted on shutdown. Statistics contain no screen pixels, clipboard
content, keys or input events.

Each stage reports its mean and nearest-rank p50/p95/p99 over the **most recent
128 observations per stage in that reporting window**. Means include all
observations; the quantiles are bounded recent samples, not full-session or
full-window percentiles. Empty quantiles are null. Counters and samples reset
between windows. Source/encoded dimensions describe the latest captured frame
in the window (zero dimensions mean no captured frame in that window).

- `capture_ms`: the complete capture call, including readback and resizing.
- `mailbox_age_ms`: capture completion to sender dequeue; excludes capture cost.
- `credit_wait_ms`: sender wait for display credit before taking the mailbox.
- `dirty_scan_ms`: tile planning, including dirty detection/refinement selection.
- `atlas_pack_and_jpeg_ms`: complete encode call, retaining the combined metric
  including wire-header preparation for comparison with older measurements.
- `atlas_pack_ms`: atlas allocation/reuse, tile copying and edge padding.
- `jpeg_ms`: JPEG encoder creation and encoding into the bounded payload writer.
  These two stage samples include only frames carrying an atlas; keepalives
  perform neither stage and do not dilute their means or quantiles.
- `writer_wait_ms`: time before starting the record write, including waiting
  for the shared authenticated-record writer and validating/reserving the frame.
- `socket_write_ms`: record encryption, socket writes and flush after acquiring
  the writer; it is not pure kernel/network time.
- `capture_start_to_ack_ms`: capture **start** through the matching drawn ACK.
  Earlier versions started this timer after capture completed, undercounting
  the end-to-end host pipeline by the capture cost. Historical measurements
  above retain that earlier definition and should not be compared directly.
- `send_to_ack_ms`: publication to the credit ledger through drawn ACK,
  including writer contention, transport, viewer processing and presentation.
  This is application feedback latency, **not network RTT**.

Completed-write counts, bytes and both write-stage samples are recorded atomically
in one reporting window. The text `write` average is `writer_wait_ms` plus
`socket_write_ms`, ending at successful flush; it excludes the telemetry update
and subsequent writer-lock release. Older text `write` measurements included that
small accounting/release overhead. Failed or partial writes contribute neither a
completed-write count nor write-stage samples.

The record includes package version and optional compile-time
`JWM_BUILD_COMMIT` (null when not supplied). For reproducible benchmark builds,
set `JWM_BUILD_COMMIT=$(git rev-parse HEAD)` in the build environment and record
whether the checkout is dirty, the backend, driver, monitor topology, refresh
rate, and full benchmark command alongside the output. No hardware performance
improvement is implied by these instrumentation changes.

### Half-drawn frames on NVIDIA

Symptom: the viewer flashes the bare, blurred wallpaper for a frame or a few,
or shows blocky black-and-white noise, most often across the top of the
screen where the clock and status bar redraw every second. The host machine's
own screen shows nothing of the kind.

One previously measured cause: on NVIDIA's proprietary driver, XRender and CopyArea reads of the
GL-composited screen can return a frame the compositor is still drawing —
its blurred backdrop before any window, or a partial blur pass. The host
reads right after every Damage notification, so it lands in that window far
more often than a periodic screen grab would. Measured on an RTX 4090 D
(driver 570) over 45 seconds of a jwm session: 20 such frames through
XRender; 0 through a core `GetImage`, which the driver synchronizes with GL
rendering; 0 in 2,700 frames of ffmpeg recording the host's screen directly.

`--scaling auto`, the default on `host`, `connect`, and `ssh`, selects compatibility
paths for this previously measured failure. On the host, it reads the screen with core `GetImage`
(not MIT-SHM `GetImage`)
and shrinks it on the CPU when the X server has the `NV-GLX` or `NV-CONTROL`
extension, and says so at session start. The CPU path averages each output
pixel's source area across threads; it took the place of a generic filter
that cost about 90 ms per 2560x1440 frame. On the same loaded desktop the
corrected path captured at about 152 ms per frame against 117 ms through
XRender, both dominated by waiting for the GPU. On the viewer, `--scaling auto`
disables XRender upscaling on NVIDIA systems, performing high-quality multi-threaded
CPU upscaling instead. This does not disable viewer SHM uploads or the cursor renderer,
and is not a guarantee against every source of top-edge noise. `--scaling gpu` restores
XRender for anyone who prefers that speed and does not see the artifacts;
`--scaling cpu` forces the direct CPU path elsewhere. On the host, CPU mode and
NVIDIA Auto also skip SHM segment creation, including after viewport or capture-area
changes. Older implementations disabled XRender but still attempted MIT-SHM; that
was not the same readback path as the core-GetImage measurement above. Software
Xvfb regressions now check actual core/SHM requests and codec-to-display top-edge
pixels. They do not reproduce or certify a physical NVIDIA driver.

- A black/invalid Composite overlay can be bypassed with
  `--capture-source root`. Root capture is a compatibility fallback and may
  omit compositor-only effects on some X servers.
- `--capture-source overlay` fails instead of falling back, which is useful
  while diagnosing Composite readback.
- “permission denied” for a key is normally fixed with `chmod 600 KEYFILE`;
  symlink key paths are intentionally rejected.
- “connection refused” means the host is not listening on that address/port or
  a firewall blocks it. Check the host's printed listener address first.
- If letters differ between machines, configure matching XKB layouts. Pointer
  and button control are independent of the keyboard layout.

## MVP limits

- one connected controller at a time: a second viewer is authenticated,
  told so, and disconnected immediately rather than left waiting;
- one active region, RandR monitor, or combined X root (the default), selected
  from at most 32 explicitly granted sources, with
  the host XFixes cursor
  shown through the negotiated independent plane or composited-video fallback; the viewer cursor is transparent during ordinary input
  forwarding and an active grab, while view-only and released-grab windows keep
  a visible local cursor; the rest of the client desktop is never affected;
- JPEG tile video plus clipboard text, images and files: no audio, drag and
  drop, lazy (paste-time) file transfer, or inter-frame codec yet;
- video uses cumulative display acknowledgements, at most two unacknowledged
  frames, and one latest queued frame; persistently slow viewers therefore
  reduce capture/encode/network work instead of accumulating stale video;
- encrypted PSK-authenticated direct-LAN transport with ephemeral forward secrecy
  in v3; no independent per-device identity or automatic key rotation;
- inter-keystroke timing is not hidden;
- X11 `x11rb`/`xcb` JWM sessions only.

These boundaries keep the first end-to-end path small and observable. A future
version can replace JPEG/TCP with a hardware video path and encrypted transport
without exposing JWM's local IPC or moving network waits into the compositor.

`jwm-tool health` prints compact `session_lock` for remote control operators.

### Input responsiveness and fitted-image clicks

Each viewer pass consumes at most 256 queued X11 events before forwarding the
collected input. Continuous motion/Expose traffic therefore cannot indefinitely
postpone that send. Adjacent motion events still coalesce, but key, button,
wheel and ReleaseAll edges keep their order, including across pass boundaries.
Input is flushed before deferred Expose/resize rendering; if a new frame is
ready, its draw absorbs that repaint instead of first drawing the old frame.
Rendering still runs on the viewer's main thread, so a slow native draw can
still delay the next input pass; no claim of measured end-to-end mouse latency
is made by the offline tests.

Clipboard forwarding also yields to the viewer loop: at most sixteen queued
clipboard records are written per pass — enough for a whole 1 MiB transfer
window of chunks plus their control records. Remaining records stay in FIFO
order; a budget yield is not a disconnect, and the clipboard worker wakes the
loop for each record it queues, so the next pass follows at once. Worker
closure is still reported once preceding records have been written,
potentially on a later pass. On the host, each clipboard record takes the
shared writer for exactly one record, so a frame can always go out between two
chunks of a large copy.

These are work-count bounds, not hard response-time bounds: sixteen records
can still total 2 MiB, and an individual socket write or native draw can
block. Copied files are read and written only by the clipboard worker, never
by the viewer loop or the host's input thread.

Letterbox bars preserve aspect ratio and are not remote desktop content.
Clicks there neither grab nor send a remote button press. A button pressed
inside the image is still released if its drag ends outside, with the pointer
clamped to the source edge; releasing an unforwarded click does not invent a
remote release. ReleaseAll clears both key and button tracking. Pointer events
are converted using the window geometry current at that event, before any
later resize or deferred rendering can reinterpret them.

When adaptive JPEG quality has recovered, unchanged tiles previously sent at
lower quality are re-encoded at the **current** quality. The two-second capture
heartbeat supplies quality repairs without adding another readback producer.
A static batch repairs at most 4,096 tiles (1,048,576 pixels / 3 MiB RGB atlas
input at the default tile size); a batch containing ordinary pixel changes
repairs at most 256 additional tiles. Both use the existing two-frame ACK credit
and authenticated writer deadline. A 2560×1600 static canvas needs four such
batches (about eight seconds under ideal capture/ACK timing) to clear one pass
of debt to the current quality. This is a scheduling bound, not a LAN benchmark;
encoding, slow ACKs and writes can extend it.

The repair cursor advances only after successful record write and flush.
Ordinary damage and repairs share one raster-ordered atlas without duplicates.
Local changes such as a blinking caret no longer reset the repair clock or
prevent unchanged text tiles from being visited. Quality-only refreshes never
downgrade sharper tiles; resizing/reconnecting establishes a fresh keyframe.
After debt is cleared the sender returns to unchanged suppression and
four-second keepalives. Input event draining remains bounded to 256 X11 events
per pass, so encoding/render work cannot indefinitely postpone input edges.

The adaptive controller still requires at least four healthy same-epoch ACKs,
a three-second observation span, low send-to-ACK RTT and no remaining pressure
before increasing quality by one point. Its sample target now follows measured
ACK cadence (bounded4..24) instead of assuming the configuredFPS still describes
an idle link. Fast bursts retain the24-sample requirement; four healthy
four-second keepalives can recover a point in about16seconds rather than96.
Epoch changes reset that history; congestion thresholds and multiplicative
quality reduction remain unchanged. These are controller/scheduling bounds,
not measured two-machine LAN results. Actual latency, text clarity and bandwidth
still require separate acceptance evidence.

## Saved connection profiles

Profiles save connection settings, not a running session. Nothing reconnects at
startup. Create a dedicated private directory first (use `$XDG_CONFIG_HOME/jwm`
instead of `$HOME/.config/jwm` when configured):

```sh
jwm-remote profile init
jwm-remote profile save office -- ssh user@office --monitor DP-0 --view-only
jwm-remote profile save lan -- connect 192.0.2.10 --key-file /absolute/private/remote.key --view-only
jwm-remote profile list
jwm-remote profile show office
jwm-remote profile run office
jwm-remote profile remove office
```

`profile --directory /absolute/private/directory ...` selects a different existing
directory. Saving never connects, reads a key, starts a host, or changes existing
permissions. Replacing a name needs `profile save NAME --replace -- ssh ...`.
`show` prints the complete settings so input/clipboard permissions can be reviewed
before `run`. The saved `connect`/`ssh` defaults match those commands: input is
permitted unless `--view-only` is present; clipboard is off unless `--clipboard`
was explicitly saved. Use `--view-only` when saving a viewing-only profile.

Supported settings include the address/SSH alias, monitor selection for SSH,
local display, input mode, grab-release keysym, clipboard consent and file limit.
SSH also accepts `--fps`, `--jpeg-quality`, and `--max-width`, with the same ranges
as the host command, and persists these host quality settings. For example:

```sh
jwm-remote profile save office --replace -- ssh user@office --monitor DP-0 --fps 24 --jpeg-quality 70 --max-width 1280 --clipboard --clipboard-file-limit 512M
jwm-remote profile run office --view-only --no-clipboard
```

Run-time profile overrides can only reduce permissions. Direct connections cannot
change a separately running host's monitor or quality. Profiles can reapply an
explicitly saved startup size and best-effort position; they do not reconnect
automatically. Extra source grants are not
stored: saving an SSH command with any `--share-monitor`, `--share-region`, or
`--share-root` flag fails rather than silently dropping it. Use `ssh` directly
for a session with extra grants. Existing profiles retain their original scope.
To request and explicitly save a local viewer client size:

```sh
jwm-remote connect desk --window-size 1280x720 --view-only
jwm-remote profile save office --replace -- ssh user@office --view-only --window-size 1280x720
jwm-remote profile run office
```

`--window-size WIDTHxHEIGHT` accepts ASCII decimal dimensions from 1 through
8192, with at most 16,777,216 pixels. This requests the local client area, not
window borders or source geometry. The window manager may tile or resize the
window differently; its actual size is then reported normally. Explicit sizes
are centered using the existing placement policy and are not reduced to fit a
smaller monitor. Without this option, the existing centered, aspect-preserving
90%-monitor fit remains unchanged.

The requested size also supplies the initial viewport before the first capture.
It remains independent of decoded-frame safety budgets: those limits still cap
video allocation and never enlarge to accommodate a requested window. The
viewer can scale or letterbox a smaller decoded frame into the requested area.
SSH uses the size only in its local viewer, never in the remote host command.
Only an explicit `profile save` stores the supplied size. No desktop geometry is
observed for persistence, and no last-selected source, extra grant, credential,
or clipboard-transfer state is saved. Old profiles without a size keep the
original default. Parsing, profile round trips and startup selection have
deterministic tests; actual window-manager placement and presentation have not
been live-X11 validated in this checkout.

An optional `--window-position X,Y` requests the local client area's root-relative
origin, using signed ASCII decimal coordinates from -32768 through 32767. For
example:

```sh
jwm-remote profile save office --replace -- ssh user@office --view-only --window-size 1280x720 --window-position 100,80
```

The saved position is advisory. Before mapping, the viewer checks current RandR
monitor rectangles clipped to the root screen. It applies the position only if
the entire client rectangle fits on the monitor containing that origin. An
origin in a monitor gap or outside the current layout, a partially off-screen
client rectangle, or unavailable monitor information produces one terminal
warning and uses local fallback placement. A requested origin on a known monitor
selects that monitor even when the rectangle must be centered instead. Otherwise
normal pointer/primary selection is preferred. An explicit oversized size is
not reduced; fallback keeps its origin visible. This does not guarantee visible
window decorations or prevent the WM from moving or tiling the window afterward.

With position alone, the initial viewport and automatic sizing use the selected
monitor rather than the pointer's monitor. Explicit size still controls the
initial viewport independently. Monitor layout is checked again when the window
is created. Only an accepted position receives a user-position WM hint; fallback
does not force a saved position. No-position behavior and the existing size-only
behavior remain unchanged. Position is saved only by explicit `profile save`,
never observed or updated when the user moves a window. It is never sent to the
SSH host. Last-source and automatic session-state restoration remain deferred.

The version-1 JSON schema rejects unknown fields and missing permission booleans.
Names are 1–64 ASCII letters, digits, hyphens or underscores. The dedicated directory
has a maximum of 128 profiles (256 total entries including crash leftovers), each
profile at most 16 KiB. Writes use a mode-0600
temporary file, sync and atomic rename under a directory lock. Existing directories
must be owned by the current user with no group/other permissions; files must be
owned, private regular files with one link. Symbolic links in the directory path
or profile filenames are rejected. The Linux implementation uses pinned directory
descriptors through `/proc/self/fd` and requires procfs. Busy stores fail rather
than wait indefinitely. No chmod or permission repair happens implicitly.

Key paths must be absolute; key contents, passwords, arbitrary SSH options and
remote executable commands cannot be stored. Use a trusted SSH config alias for
advanced SSH configuration. A saved profile is local configuration, not an import
format for untrusted third parties. A crash before atomic rename can leave a
`.write-*` temporary file: bounded, private, owned regular files with the exact
temporary filename format are ignored under the directory lock, not deleted.
Other unexpected or unsafe entries fail closed. `profile init` explicitly creates
missing directories privately and never changes existing directory permissions.

### Explicit discrete Wayland scroll (v18)

Version 18 extended the stream profile to 29 bytes; v19 adds the video-only
copy selector for a total of 30 bytes. Byte 28 remains a strict boolean scroll capability,
independent of byte 27's pointer permission. It must be false unless pointer is
also true. Both `host --allow-wayland-pointer --allow-wayland-scroll` and
`connect --wayland-pointer --wayland-scroll` are required. A missing or unsolicited
scroll grant terminates negotiation, with no fallback to older peers or a native
v1 lease. Native manager v2 creates an explicitly scroll-enabled lease; the old
lease request remains motion/click only. Action kinds 2 and 3 are one up/down
notch respectively, keeping the 45-byte scoped action format. Each consumes a
fresh presented frame; waiting/held-click wheel input is discarded, never queued.
No horizontal/smooth scroll, drag, new input permission, or lease extension is added.

### Experimental vertical scroll-copy codec (v19)

`jwm-remote host --scroll-copy` explicitly enables a CPU tile-codec optimization.
It is off by default and is not saved in profiles or implicitly enabled by SSH.
The flag grants **no input permission**: `--allow-wayland-scroll` remains a
separate, explicitly authorized single-notch input capability.

Both endpoints must use application protocol v19. Earlier peers are rejected
at negotiation, not silently upgraded. Codec support is negotiated separately
from keyboard, pointer, clipboard and scroll permissions.

A candidate copies a full-width vertical overlap by an integral displacement
of ±1 through ±64 encoded-image pixels, strictly less than the encoded image
height. This does not move the source window or change input coordinates. The encoder
requires at least half the image to overlap and an exact match of every raw RGB
pixel in that overlap. Newly exposed pixels are sent through the existing tile
path. Partial-column scrolling, fixed toolbars, unrelated changes and unsafe
size changes do not qualify for that copy; ordinary tile encoding remains the
fallback. Copy records bind to the exact previous decoded sequence and current
source/geometry scope; stale, missing or cross-scope bases must not be applied.

Copied pixels are the existing decoded pixels, not re-quantized JPEG. This does
not guarantee lower image error than a fresh JPEG update. The experiment is
software-only; no GPU acceleration, physical presentation latency, network
latency or general bandwidth improvement is promised. Bounded offline A/B
measurements compare the same deterministic RGB frames and JPEG quality.

### Explicit native Wayland viewer keyboard mode

With a `remote-wayland` build, native viewing remains an explicit choice:
`connect HOST --viewer-backend wayland --view-only`. The alternative
`connect HOST --viewer-backend wayland --wayland-keyboard` requests only the
existing scoped keyboard lease, and requires the host's separate
`--wayland-select --once --allow-wayland-keyboard` consent. Use the same private
session key at both endpoints. A third alternative, `connect HOST --viewer-backend wayland --wayland-pointer`,
requests the existing motion/atomic-left-click lease with separate host
`--allow-wayland-pointer` consent. Alongside these modes, explicit unified control and one-shot text are available. Exactly one of the five native viewer modes is
required; they cannot be combined, and omitting all native modes is rejected; choosing Wayland never implicitly
permits legacy input.

Keyboard admission retains full keymap compatibility and the exact selected
surface/receiver identity. It lasts at most 30 seconds without renewal and is
revoked on the existing focus, local takeover, lock and lifecycle boundaries.
The native viewer does not accept clipboard, grabs,
source resize/follow, automatic reconnect or saved-profile authorization.
`--viewer-wayland-display` names a native socket; there is no X11 fallback.
See [native Wayland source safety](remote-wayland-source.md) for limits and
commands. This does not change the network protocol or imply physical-display,
two-computer or latency acceptance.

While scoped keyboard authorization is waiting for the host grant, do not type
into the viewer. Any actual key edge during that interval, even a balanced
press/release pair, cancels the attempt and terminates the session rather than
being silently dropped and later granting control. This safety rule applies to
both the X11 and native Wayland scoped-keyboard viewers.

In the native Wayland keyboard viewer, local keyboard focus loss is fail-closed:
the session terminates rather than reusing the lease after focus returns. Start
a new explicitly authorized session to regain control.

Native pointer mode observes a new local left-button press followed by its release
before emitting an atomic click. An unmatched release cancels the candidate; it
never synthesizes a press. Wayland enter events do not report an initial held-button
snapshot, so this is not proof that all buttons were initially released. Non-left
buttons, unsupported axis events or leaving the viewer surface permanently revoke the pointer
lease, while video may continue read-only. Returning does not automatically
regrant input. Unlike native keyboard focus loss, this need not close the session. There is no
drag or button chord support. Optional native scrolling requires both viewer
`--wayland-scroll` and host `--allow-wayland-scroll` in addition to pointer consent.
Only protocol-v5–7 Wheel-source vertical axis_discrete ±1, with a matching finite
nonzero axis and a complete frame, is accepted. No v8 value120, smooth/finger or
horizontal scrolling, multi-notch batches or mixed button/axis input is enabled.
Waiting scroll is discarded, not queued for a later Ready. Every motion/click still needs the matching
fresh, genuinely presented frame and host Ready ticket; the 30-second lease is
not renewed. Default read-only viewing does not bind input devices.


### Explicit native pointer-only plus static lossless (application v27)

This combination was introduced in v27; both peers must now use the same current application version. A host may explicitly combine
`--wayland-select --once --allow-wayland-pointer --allow-static-lossless`, and
the viewer must separately request
`connect HOST --viewer-backend wayland --wayland-pointer --static-lossless`.
Do not add `--view-only`. This default-off combination permits only the existing
motion/atomic-left-click lease, not keyboard, unified control, scroll, text,
clipboard, grab, popup capture or resize-follow. It grants no new input actions.

Repair cannot generate authorization: another actionable scope requires a new
native proof and actual presentation acknowledgment. The original 30-second
lease is not renewed. Candidate `9b3105` passed general, Wayland and X11 CI,
including all seven new module tests; all four baseline mainline workflows
subsequently passed. This does not validate the separate natural-expiry-to-read-only
candidate, whose new tests have not yet run. The owned software-GL 800×600 test reached full-frame exactness
in eight batches, then performed an actual click. This does not imply physical
hardware acceptance or common-desktop full-frame convergence: 1080p needs at
least 32 batches and 64 seconds in the ideal budget, longer than the lease.
See [pointer plus lossless boundaries and evidence](remote-pointer-lossless-preparation.md)
for observed times, permission restrictions and the unchanged cooldown limits.
