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
one player on the bus — `p` hands the row to the next player and `o` opens
the Players picker (see [Which player wins](#which-player-wins)). The row
hides the skip glyphs a player says it cannot honor (`CanGoNext` /
`CanGoPrevious`), and disappears entirely when no player is running. The
pointer mirrors the keyboard on those glyphs: a click on the previous /
next arrow skips, a click on the title or status icon play/pauses, and —
with more than one player — a click on the trailing `· p ‹next›` hint
cycles players while a click on the trailing `· o` opens the Players
picker. When the player reports both a position and a length, the
row shows `m:ss / m:ss` (`h:mm:ss` past an hour) — clamped to the track's
length for display, refreshed on the bridge's sweep, holding the last
polled position while paused, and reset on a track change; streams and
players that don't report both show no suffix, never a placeholder. When
the player also reports `CanSeek`, a click on that suffix seeks to the
pointed fraction of the track (the pointer twin of an absolute seek); the
title and status icon still play/pause. Without `CanSeek` — or on an old
bridge that never sent the flag — the suffix stays display-only.

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

A confirmed audio-device switch — picker Enter after the worker's re-read, or
a successful `set_audio_device` — raises a labeled card with the device
description (speaker glyph for outputs, microphone glyph for inputs), truncated
like a media label. Queueing and a failed re-read stay quiet; see
[control center](control-center.md#audio-device-pickers). A concurrent
volume/mic correction shares the flush path but not the pending slot: when
both are ready the device card is shown.

## Microphone mute

`XF86AudioMicMute` (`toggle_mic_mute`, bound by default) toggles the default
microphone's mute against `@DEFAULT_AUDIO_SOURCE@`, through the same
`wpctl` / `pactl` / `amixer` fallback chain the volume control settles on,
and rides the same controls worker the volume and brightness keys use: the
press is an event, the shown state flips optimistically, and the worker's
read-back corrects it if the toggle did not take. The feedback is a labeled OSD card — `Microphone
Muted` / `Microphone Unmuted` with fa-microphone(-slash) icons — which,
unlike the volume card, carries no bar: the flag is the whole story.

The flag has three more consumers. The control center's Input row is the
indicator: while the default source is muted it wears the slashed
microphone icon the OSD uses, and an unmuted or never-read flag draws the
row exactly as before; an open control center repaints when a read-back
corrects or reverts the shown state. A click on that microphone glyph —
or `m` while the Input row is selected — toggles mute the same way the
key does (OSD included); a click on the rest of the row still opens the
input device picker. `set_mic_mute {"muted": bool}`
sets the flag over IPC — queued, like the volume keys, and deliberately
unlike `set_audio_device`'s synchronous confirmed reply: the `ok` ack is
immediate and the OSD draws the optimistic estimate, then the worker's
read-back confirms or corrects it (the OSD refreshes in place and the
Input row follows). A non-boolean `muted` is rejected with
`set_mic_mute: expected boolean field 'muted'`, and a session with no
working audio tool gets the key path's own answer,
`no working audio control (wpctl/pactl/amixer)`. The command is advertised
through `get_capabilities`. And bars can follow without scraping the OSD:
`get_mic_mute` answers `{ "muted": true|false|null }` from the same
cached flag (`null` means never read — never invent unmuted), warming the
coalesced control snapshot first like `get_audio_devices`, while every
shown-flag change publishes `audio/mic` on the `audio` topic with a bool
payload (optimistic set/toggle, adopt, and revert-to-a-bool; clearing back
to unread is silent).

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
(`· p spotify`), then a `· o` hint for the Players picker, and pressing `p`
— or clicking that `· p` hint — pins the row — and the transport keys with
it — to that next player, wrapping around the list. Pressing `o` — or
clicking `· o` — opens a Players picker listing every player the sweep
saw (filled marker on the active one). When the bridge sends
`player_details`, each row prefers the player's MPRIS `Identity` over the
bus suffix and trails a Playing/Paused/Stopped icon; without details (an
old bridge) the rows stay suffix-only. Enter / click pins by bus suffix
and returns to the hub. A click on the previous / next glyph skips; a
click on the title or status icon still play/pauses. The bridge holds the
pin while the pinned player's bus name is alive and re-publishes its
state, so the switch raises the media OSD like any track change; when the
pinned player exits while another player remains, the pin clears itself
and the ranking takes the row back. An empty bus keeps the pin so a later
launch of the same player — or a restart that starts the bridge before
any player — can reclaim it. The choice is also written to
`$XDG_STATE_HOME/jwm/mpris-pin` (else `~/.local/state/jwm/mpris-pin`), so
a bridge restart restores it; a dead or empty select clears the file. A
single-player session is untouched: `p` and `o` are no-ops and the
row carries no hint. The lock screen's now-playing row never grows the
hint either — it is a control, and the lock shows none.

Player start/stop is picked up from bus name-owner changes; track changes are
picked up by a 3-second sweep.

## IPC

- `set_media_status` — what the bridge pushes: `player`, `identity`, `status`
  (`Playing`/`Paused`/`Stopped`), `title`, `artist`, `can_go_next`,
  `can_go_previous`, the append-only `can_seek` bool, the append-only
  `position_us` / `length_us` microsecond
  fields (nullable), the append-only `players` string list naming every
  player the sweep saw (in sweep order — the cycle/select key), and the
  append-only `player_details` array of `{player, identity?, status?}` in
  the same order so the Players picker can show Identity and a status cue.
  Mixed old/new bridge↔jwm pairs are tolerated; a missing `players` list
  reads as a single-player session, and a missing `player_details` keeps
  suffix-only picker rows. A missing or null `player` clears the state,
  which is how the bridge reports that every player went away.
- `media_control` — `{"action": "play_pause" | "next" | "previous" | "stop" |
  "seek"}`. Seek carries `position_us` (absolute microseconds).
  `toggle`, `playpause`, and `prev` are accepted aliases.
- `set_mic_mute` — `{"muted": bool}` sets the default microphone's mute flag.
  Queued, not confirmed: the `ok` ack is immediate and the OSD draws the
  optimistic estimate, with the controls worker's read-back confirming or
  correcting it after — deliberately unlike `set_audio_device`'s synchronous
  re-read reply (which raises a named device OSD only after the switch took).
  See [Microphone mute](#microphone-mute).
- `get_mic_mute` — `{ "muted": true|false|null }` from the cached flag after
  warming the coalesced control snapshot. `null` means never read.
- the `audio` subscription topic carries `audio/devices` and `audio/mic`
  (bool `muted` whenever the shown flag becomes a known bool).
- `get_media_status` — the current state plus the rendered `label`, a
  pre-formatted nullable `position_label`, the append-only `players` list
  (same order as the bridge push), and append-only `player_details` (status
  as `playing`/`paused`/`stopped`), so bars don't reimplement the clamping
  or scrape the control-center row for a picker.
- the `media` subscription topic carries `media/status` and `media/command`.
  `media/status` also carries the append-only `players` and `player_details`
  fields.

Bars can subscribe to `media/status` for a now-playing widget without talking
to MPRIS themselves. See [notifications](notifications.md) for building and
installing `jwm-bridge`, which serves both features from one process.
