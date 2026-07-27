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
`Left`/`Right` skip tracks, `Return` toggles playback. The row hides the
skip glyphs a player says it cannot honor (`CanGoNext` / `CanGoPrevious`),
and disappears entirely when no player is running.

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

## Which player wins

The bridge ranks every `org.mpris.MediaPlayer2.*` name on the bus: playing
beats paused beats stopped, and ties keep the earlier name so the choice does
not flap between two idle players. It re-reads the ranking on every control
request, so pressing play after switching players drives the one now in front.

Player start/stop is picked up from bus name-owner changes; track changes are
picked up by a 3-second sweep.

## IPC

- `set_media_status` — what the bridge pushes: `player`, `identity`, `status`
  (`Playing`/`Paused`/`Stopped`), `title`, `artist`, `can_go_next`,
  `can_go_previous`. A missing or null `player` clears the state, which is how
  the bridge reports that every player went away.
- `media_control` — `{"action": "play_pause" | "next" | "previous" | "stop"}`.
  `toggle`, `playpause`, and `prev` are accepted aliases.
- `get_media_status` — the current state plus the rendered `label`.
- the `media` subscription topic carries `media/status` and `media/command`.

Bars can subscribe to `media/status` for a now-playing widget without talking
to MPRIS themselves. See [notifications](notifications.md) for building and
installing `jwm-bridge`, which serves both features from one process.
