# Media controls

The shell shows what is playing and drives it: a transport row at the top of
the control center, media keys bound out of the box, and an OSD card when the
track changes.

JWM itself never talks to MPRIS. `jwm-bridge` watches the session bus and
pushes the active player's state in over IPC; JWM broadcasts control requests
back out and the bridge turns them into method calls. The compositor keeps no
bus connection, so a hung player cannot stall a frame.

```
player --MPRIS--> jwm-bridge --set_media_status--> jwm --> control center row
                             <--media/command----          media OSD card
```

## Keys

| Key | Action |
| --- | --- |
| `XF86AudioPlay` | `media_play_pause` |
| `XF86AudioNext` | `media_next` |
| `XF86AudioPrev` | `media_previous` |
| — | `media_stop` is bindable but unbound by default |

In the control center the media row is first when a player is running:
`Left`/`Right` skip tracks, `Return` toggles playback, and — with more than
one player on the bus — `p` hands the row to the next player (see
[Which player wins](#which-player-wins)). The row hides the
skip glyphs a player says it cannot honor (`CanGoNext` / `CanGoPrevious`),
and disappears entirely when no player is running. When the player reports
both a position and a length, the row shows `m:ss / m:ss` (`h:mm:ss` past
an hour) — clamped to the track's length for display, refreshed on the
bridge's sweep, holding the last polled position while paused, and reset on
a track change; streams and players that don't report both show no suffix,
never a placeholder. It is display-only — no seeking.

A media key on a session with no player reports `no media player is running`
rather than failing silently.

## The OSD card

A *track change* — a different track, or a switch to another player — raises
the bottom-center OSD with the status icon and `Title — Artist`. Pausing and
resuming the same track does not, so the card stays out of the way during
ordinary transport use. The media card is wider than the volume/brightness
cards and carries no progress bar.

Media keys also echo the current track on the OSD immediately, so a keypress
gives feedback before the player has answered.

## Microphone mute

`XF86AudioMicMute` (`toggle_mic_mute`, bound by default) toggles the default
microphone's mute against `@DEFAULT_AUDIO_SOURCE@`, through the same
`wpctl` / `pactl` / `amixer` fallback chain the volume control settles on,
and rides the same controls worker the volume and brightness keys use: the
press is an event, the shown state flips optimistically, and the worker's
read-back corrects it if the toggle did not take. The feedback is a labeled OSD card — `Microphone
Muted` / `Microphone Unmuted` with fa-microphone(-slash) icons — which,
unlike the volume card, carries no bar: the flag is the whole story.

The flag has two more consumers. The control center's Input row is the
indicator: while the default source is muted it wears the slashed
microphone icon the OSD uses, and an unmuted or never-read flag draws the
row exactly as before; an open control center repaints when a read-back
corrects or reverts the shown state. And `set_mic_mute {"muted": bool}`
sets the flag over IPC — queued, like the volume keys, and deliberately
unlike `set_audio_device`'s synchronous confirmed reply: the `ok` ack is
immediate and the OSD draws the optimistic estimate, then the worker's
read-back confirms or corrects it (the OSD refreshes in place and the
Input row follows). A non-boolean `muted` is rejected with
`set_mic_mute: expected boolean field 'muted'`, and a session with no
working audio tool gets the key path's own answer,
`no working audio control (wpctl/pactl/amixer)`. The command is advertised
through `get_capabilities`.

One deliberate absence stands: the key is *not* in the lock-screen media
passthrough (unmuting a microphone while locked is a privacy risk, so the
passthrough stays at its ten keysyms).

## Which player wins

The bridge ranks every `org.mpris.MediaPlayer2.*` name on the bus: playing
beats paused beats stopped, and ties keep the earlier name so the choice does
not flap between two idle players. It re-reads the ranking on every control
request, so pressing play after switching players drives the one now in front.

The ranking loses to a pin. With more than one player running, the media row
ends with a `· p ‹next player›` hint naming what the key would switch to
(`· p spotify`), and pressing `p` pins the row — and the transport keys with
it — to that next player, wrapping around the list. The bridge holds the pin
while the pinned player's bus name is alive and re-publishes its state, so
the switch raises the media OSD like any track change; when the pinned
player exits, the pin clears itself and the ranking takes the row back. A
single-player session is untouched: `p` is a no-op and the row carries no
hint. The lock screen's now-playing row never grows the hint either — it is
a control, and the lock shows none.

Player start/stop is picked up from bus name-owner changes; track changes are
picked up by a 3-second sweep.

## IPC

- `set_media_status` — what the bridge pushes: `player`, `identity`, `status`
  (`Playing`/`Paused`/`Stopped`), `title`, `artist`, `can_go_next`,
  `can_go_previous`, the append-only `position_us` / `length_us` microsecond
  fields (nullable), and the append-only `players` list naming every player
  the sweep saw, in sweep order. Mixed old/new bridge↔jwm pairs are
  tolerated; a missing list reads as a single-player session. A missing
  or null `player` clears the state, which is how
  the bridge reports that every player went away.
- `media_control` — `{"action": "play_pause" | "next" | "previous" | "stop"}`.
  `toggle`, `playpause`, and `prev` are accepted aliases.
- `set_mic_mute` — `{"muted": bool}` sets the default microphone's mute flag.
  Queued, not confirmed: the `ok` ack is immediate and the OSD draws the
  optimistic estimate, with the controls worker's read-back confirming or
  correcting it after — deliberately unlike `set_audio_device`'s synchronous
  re-read reply. See [Microphone mute](#microphone-mute).
- `get_media_status` — the current state plus the rendered `label` and a
  pre-formatted nullable `position_label`, so bars don't reimplement the
  clamping.
- the `media` subscription topic carries `media/status` and `media/command`.

Bars can subscribe to `media/status` for a now-playing widget without talking
to MPRIS themselves. See [notifications](notifications.md) for building and
installing `jwm-bridge`, which serves both features from one process.
