# Adopting the 0.7 network and media states

Version 0.7 keeps every 0.6 surface and grows the semantic model by two
states. Nothing was removed; consumers that only repin build unchanged and
simply do not show the new pills until they enable a provider.

## New model surface

- `NetworkState`: primary interface, connectivity, and per-second rx/tx rates.
  Rates stay unavailable until a provider has two samples — they never render
  as a healthy zero.
- `MediaState` + `MediaPlayback`: MPRIS-shaped now-playing state. A stopped
  player with no track metadata normalizes to `MediaState::inactive()`.
- `BarEvent::Network` / `BarEvent::Media`, `DirtyBits::NETWORK_CHANGED` /
  `MEDIA_CHANGED`, and new `network` / `media` fields on `BarView` (borrowed)
  and `BarSnapshot` (owned, on the wire).
- `FrontendPartitions::NETWORK` / `MEDIA` for web stores;
  `snapshot_changes` classifies both fields.
- `display::format_transfer_rate` for compact `1.5 MiB/s` values.

## Presentation

`STATUS_ORDER_RIGHT_TO_LEFT` grows to 11 entries: `Network` sits between
`Audio` and `Memory`; `Media` sits left of `Cpu`, right of `Monitor`. The
network pill shows `↓rate ↑rate` with explicit unknowns and an offline icon
when disconnected. The media pill appears only while a player is active, so
inactive hosts lose no bar space. `PresentationVisibility` gains `network` /
`media` toggles and `PresentationLabels` gains `network`, `network_offline`,
`media_playing`, and `media_paused` (with Nerd Font values in the icon-set
mapping). Both pills are display-only; playback control actions are a later
protocol addition.

## Network provider (`provider-network-sysfs`)

A dependency-free sysfs provider integrated into `BarRuntime::tick` like the
battery provider — enabling the feature is the entire migration:

```toml
xbar_core = { git = "…", tag = "v0.7.0", default-features = false, features = [
  "provider-network-sysfs", # …existing features
] }
```

The primary interface is the alphabetically first non-loopback interface with
`operstate == up`, keeping the selection deterministic. Counter resets after
an interface bounce surface as one unavailable sample, not a wrapped rate.
`xbar_tauri` forwards the feature under the same name.

## Media provider (`xbar_dbus_providers` 0.2)

`MprisMediaProvider` polls the session bus, discovers the first
`org.mpris.MediaPlayer2.*` name each sample, and reduces `PlaybackStatus` +
`Metadata` to a `MediaState`. Media is host-opt-in because it costs a zbus
dependency: a host polls the provider and feeds
`runtime.apply_event(BarEvent::Media(state))` on its tick. Bars that do not
wire it keep an inactive (hidden) media pill.

## Wire compatibility

`BarSnapshot` JSON gains `network` and `media` objects. Existing web frontends
that ignore unknown fields keep working; `DirtyBits` deserialization masks the
new bits on old consumers, so mixed-version replay stays safe.
